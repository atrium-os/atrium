//! The launch question (personas.md §3): an app that runs once per persona,
//! launched by a person with a named persona, asks which. A small dialog of
//! its own: "Open Mail in", a row per persona (Personal first) with its
//! colour, the one last used preselected. Enter or a press opens it there;
//! Escape, or the dialog losing focus, opens nothing.
//!
//! The persona's colour here is only a hint for choosing: what proves an
//! instance's persona afterwards is frescod's chip and forum-wm's frame.

use std::cell::RefCell;
use std::rc::Rc;

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{Color, Mutable, TextStyle};
use portcullis_ipc::PersonaOption;

fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

/// The dialog's width, its padding, a row's height and the gap between rows,
/// in points at 100% text.
pub const W: f32 = 320.0;
const PAD: f32 = 20.0;
const TITLE_H: f32 = 40.0;
const ROW_H: f32 = 40.0;
const ROW_GAP: f32 = 6.0;

pub const KEY_1: u16 = 0x1E;
pub const KEY_9: u16 = 0x26;
pub const KEY_ENTER: u16 = 0x28;
pub const KEY_ESCAPE: u16 = 0x29;
pub const KEY_DOWN: u16 = 0x51;
pub const KEY_UP: u16 = 0x52;
pub const KEY_KP_ENTER: u16 = 0x58;

/// What the person answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Open it in this persona (`personal` for Personal).
    In(String),
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub app_name: String,
    /// Personal first, as portcullisd lists them.
    pub personas: Vec<PersonaOption>,
    pub selected: usize,
    pub answer: Option<Answer>,
}

impl Question {
    /// The question for `app_name`, or `None` when there is nothing to ask:
    /// only Personal, or an app that spans personas.
    pub fn new(app_name: &str, personas: Vec<PersonaOption>, last: Option<&str>, spans: bool) -> Option<Question> {
        if spans || personas.len() < 2 {
            return None;
        }
        let selected = last.and_then(|l| personas.iter().position(|p| p.name == l)).unwrap_or(0);
        Some(Question { app_name: app_name.into(), personas, selected, answer: None })
    }

    pub fn title(&self) -> String {
        format!("Open {} in", self.app_name)
    }

    /// A key, by HID usage: arrows move, Enter opens, 1–9 open that row,
    /// Escape cancels.
    pub fn key(&mut self, hid: u16) {
        let n = self.personas.len();
        match hid {
            KEY_UP => self.selected = (self.selected + n - 1) % n,
            KEY_DOWN => self.selected = (self.selected + 1) % n,
            KEY_ENTER | KEY_KP_ENTER => self.pick(self.selected),
            KEY_ESCAPE => { self.answer.get_or_insert(Answer::Cancel); }
            k if (KEY_1..=KEY_9).contains(&k) => {
                let i = (k - KEY_1) as usize;
                if i < n {
                    self.pick(i);
                }
            }
            _ => {}
        }
    }

    pub fn pick(&mut self, i: usize) {
        if let Some(p) = self.personas.get(i) {
            self.selected = i;
            self.answer.get_or_insert(Answer::In(p.name.clone()));
        }
    }
}

/// The dialog's size in points at the current text size.
pub fn size(rows: usize) -> (f32, f32) {
    let n = rows as f32;
    (em(W), em(PAD * 2.0 + TITLE_H) + n * em(ROW_H) + (n - 1.0).max(0.0) * em(ROW_GAP))
}

/// Row `i`'s rect in points.
pub fn row(i: usize, w: f32) -> Rect {
    Rect::new(em(PAD), em(PAD + TITLE_H) + i as f32 * (em(ROW_H) + em(ROW_GAP)), w - em(PAD) * 2.0, em(ROW_H))
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn swatch(p: &PersonaOption) -> Option<Color> {
    let [r, g, b] = fresco_protocol::persona::rgb(p.colour.as_deref()?)?;
    Some(Color::from_hex(&format!("#{r:02x}{g:02x}{b:02x}")))
}

pub struct PersonaView {
    pub q: Mutable<Question>,
    pub size: Mutable<(f32, f32)>,
    pub picks: Rc<RefCell<Vec<usize>>>,
}

impl View for PersonaView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let q = self.q.get_cloned();
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_elevated(), radius: radius::LG });
        let th = em(type_size::MD) * 1.5;
        ctx.add(Node::Text { rect: Rect::new(em(PAD), em(PAD) + (em(TITLE_H) - th) / 2.0 - em(4.0), 0.0, th), content: q.title(), style: sans(type_size::MD, Weight::Semibold, t.text_primary()) });
        for (i, p) in q.personas.iter().enumerate() {
            let r = row(i, w);
            ctx.key(200 + i as u64);
            let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::MD });
            let fill = if i == q.selected {
                t.selection_bg()
            } else if ctx.is_pressed(id) {
                t.state_pressed()
            } else if ctx.is_hovered(id) {
                t.state_hover()
            } else {
                t.bg_surface()
            };
            if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                *f = fill;
            }
            let d = em(12.0);
            let sx = r.x() + em(12.0);
            match swatch(p) {
                Some(c) => {
                    ctx.add(Node::Rect { rect: Rect::new(sx, r.y() + (r.h() - d) / 2.0, d, d), fill: c, radius: radius::PILL });
                }
                // Personal: a ring, no colour of its own.
                None => {
                    ctx.add(Node::Rect { rect: Rect::new(sx, r.y() + (r.h() - d) / 2.0, d, d), fill: t.border_strong(), radius: radius::PILL });
                    let k = em(2.0);
                    ctx.add(Node::Rect { rect: Rect::new(sx + k, r.y() + (r.h() - d) / 2.0 + k, d - 2.0 * k, d - 2.0 * k), fill: fill_or(fill, t.bg_surface()), radius: radius::PILL });
                }
            }
            let lh = em(type_size::SM) * 1.5;
            let label = format!("{}  {}", i + 1, fresco_protocol::persona::label(&p.name));
            ctx.add(Node::Text { rect: Rect::new(sx + d + em(10.0), r.y() + (r.h() - lh) / 2.0, 0.0, lh), content: label, style: sans(type_size::SM, Weight::Medium, t.text_primary()) });
            let picks = self.picks.clone();
            ctx.on_click(id, move || picks.borrow_mut().push(i));
            ctx.pop();
        }
        ctx.pop();
    }
}

/// A transparent row shows the surface under it.
fn fill_or(c: Color, under: Color) -> Color {
    if c.a == 0.0 { under } else { c }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: &str, c: Option<&str>) -> PersonaOption {
        PersonaOption { name: n.into(), colour: c.map(String::from) }
    }

    fn two() -> Vec<PersonaOption> {
        vec![p("personal", None), p("work", Some("blue"))]
    }

    #[test]
    fn nothing_asked_with_only_personal_or_for_an_app_that_spans() {
        assert_eq!(Question::new("Mail", vec![p("personal", None)], None, false), None);
        assert_eq!(Question::new("Files", two(), None, true), None);
        assert!(Question::new("Mail", two(), None, false).is_some());
    }

    #[test]
    fn the_last_used_is_preselected_and_enter_takes_it() {
        let mut q = Question::new("Mail", two(), Some("work"), false).unwrap();
        assert_eq!(q.selected, 1);
        q.key(KEY_ENTER);
        assert_eq!(q.answer, Some(Answer::In("work".into())));
        // A forgotten persona: Personal.
        let q = Question::new("Mail", two(), Some("school"), false).unwrap();
        assert_eq!(q.selected, 0);
    }

    #[test]
    fn keys_move_pick_and_cancel_and_the_first_answer_stands() {
        let mut q = Question::new("Mail", two(), None, false).unwrap();
        q.key(KEY_UP);
        assert_eq!(q.selected, 1);
        q.key(KEY_DOWN);
        assert_eq!(q.selected, 0);
        q.key(KEY_9);
        assert_eq!(q.answer, None, "no ninth persona");
        q.key(KEY_1 + 1);
        assert_eq!(q.answer, Some(Answer::In("work".into())));
        q.key(KEY_ESCAPE);
        assert_eq!(q.answer, Some(Answer::In("work".into())));
        let mut q = Question::new("Mail", two(), None, false).unwrap();
        q.key(KEY_ESCAPE);
        assert_eq!(q.answer, Some(Answer::Cancel));
    }

    #[test]
    fn rows_fit_the_dialog() {
        let (w, h) = size(3);
        let r = row(2, w);
        assert!(r.y() + r.h() <= h - em(PAD) + 0.01);
        assert!(row(0, w).y() >= em(PAD + TITLE_H) - 0.01);
    }
}
