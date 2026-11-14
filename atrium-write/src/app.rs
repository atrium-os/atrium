//! The window's contents: a toolbar, the pages (Pergola's `RichTextEditor`), a
//! status line.
//!
//! The view owns no I/O: buttons queue a [`Cmd`]; file commands go to the event
//! loop (the binary, which talks to the picker), editing commands run in
//! [`Write::take_cmds`]. The same view runs headless in tests and examples.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use officina_text::model::{Align, ListKind};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, MenuButton, MenuItem, MenuState, ScrollState};
use pergola::{Color, Mutable, TextStyle};
use pergola_richtext::editor::Toggle;
use pergola_richtext::RichTextView;

use crate::doc::Doc;

pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const STATUS_H: f32 = 26.0;

/// The paragraph styles the Style menu offers, in order.
pub const STYLES: [&str; 5] = ["Normal", "Title", "Heading 1", "Heading 2", "Heading 3"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    Toggle(Toggle),
    Align(Align),
    Style(usize),
    List(ListKind),
    PageNumbers,
}

#[derive(Debug, Clone, Default)]
pub struct Ui {
    pub asking: bool,
}

/// Cheap to clone: every field is shared.
#[derive(Clone)]
pub struct Write {
    pub doc: Rc<RefCell<Doc>>,
    pub scroll: Mutable<ScrollState>,
    pub ui: Mutable<Ui>,
    pub rev: Mutable<u64>,
    pub cmds: Arc<Mutex<Vec<Cmd>>>,
    pub size: Rc<Cell<(f32, f32)>>,
    pub style_menu: Mutable<MenuState>,
}

impl Write {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        Self { doc: Rc::new(RefCell::new(doc)), scroll, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), style_menu: Mutable::new(MenuState::default()) }
    }

    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new document on screen, at its top.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        let mut st = self.scroll.get();
        st.physics.set_position(0.0);
        self.scroll.set(st);
        self.changed();
        old
    }

    pub fn pages_rect(w: f32, h: f32) -> Rect {
        Rect::new(0.0, TOOLBAR_H, w, (h - TOOLBAR_H - STATUS_H).max(40.0))
    }

    /// Runs the editing commands queued since the last call and journals what
    /// the editor did; returns the file commands for the event loop.
    pub fn take_cmds(&self) -> Vec<Cmd> {
        let cmds = std::mem::take(&mut *self.cmds.lock().unwrap());
        let out: Vec<Cmd> = cmds
            .into_iter()
            .filter(|&c| {
                let d = self.doc.borrow();
                let mut e = d.ed.borrow_mut();
                match c {
                    Cmd::Undo => {
                        e.undo();
                    }
                    Cmd::Redo => {
                        e.redo();
                    }
                    Cmd::Toggle(t) => e.toggle(t),
                    Cmd::Align(a) => e.align(a),
                    Cmd::Style(i) => e.style_named(STYLES[i.min(STYLES.len() - 1)]),
                    Cmd::List(k) => e.toggle_list(k),
                    Cmd::PageNumbers => e.toggle_page_numbers(),
                    _ => return true,
                }
                drop(e);
                drop(d);
                self.changed();
                false
            })
            .collect();
        self.sync();
        out
    }

    /// Journals the editor's actions; marks the document edited.
    pub fn sync(&self) {
        if self.doc.borrow_mut().sync_log() {
            self.changed();
        }
    }
}

impl View for Write {
    fn render(&self, ctx: &mut Ctx) {
        let _ = self.rev.get();
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // Toolbar: files, history, character and paragraph formatting.
        let mut x = 12.0;
        let mut button = |ctx: &mut Ctx, label: &str, cmd: Cmd, wd: f32, gap: f32| {
            let cmds = self.cmds.clone();
            Button::secondary(label).at(x, 8.0).width(wd).on_click(move || cmds.lock().unwrap().push(cmd)).render(ctx);
            x += wd + gap;
        };
        button(ctx, "Open…", Cmd::Open, 72.0, 6.0);
        button(ctx, "Save", Cmd::Save, 60.0, 6.0);
        button(ctx, "Save As…", Cmd::SaveAs, 88.0, 16.0);
        button(ctx, "Undo", Cmd::Undo, 60.0, 6.0);
        button(ctx, "Redo", Cmd::Redo, 60.0, 16.0);
        button(ctx, "B", Cmd::Toggle(Toggle::Bold), 36.0, 4.0);
        button(ctx, "I", Cmd::Toggle(Toggle::Italic), 36.0, 4.0);
        button(ctx, "U", Cmd::Toggle(Toggle::Underline), 36.0, 16.0);
        button(ctx, "• List", Cmd::List(ListKind::Bullet), 64.0, 4.0);
        button(ctx, "1. List", Cmd::List(ListKind::Numbered), 68.0, 4.0);
        button(ctx, "Page #", Cmd::PageNumbers, 68.0, 16.0);
        let (style_name, cur_align) = {
            let d = self.doc.borrow();
            let e = d.ed.borrow();
            let p = e.doc.para(e.caret.block);
            let name = p.and_then(|p| e.doc.styles.styles.get(p.style as usize)).map(|s| s.name.to_string()).unwrap_or_default();
            let align = p.map(|p| e.doc.styles.para_format(p.style, &p.props).0.align).unwrap_or_default();
            (name, align)
        };
        {
            let items: Vec<MenuItem> = STYLES.iter().map(|s| MenuItem::new(*s).radio(*s == style_name)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new(if style_name.is_empty() { "Style".to_string() } else { style_name.clone() }, self.style_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Style(i))).at(x, 8.0).width(120.0).render(ctx);
            x += 136.0;
        }
        for (label, a) in [("Left", Align::Start), ("Centre", Align::Center), ("Right", Align::End), ("Justify", Align::Justify)] {
            let wd = if label == "Justify" { 64.0 } else { 56.0 };
            let cmds = self.cmds.clone();
            let b = if a == cur_align { Button::primary(label) } else { Button::secondary(label) };
            b.at(x, 8.0).width(wd).on_click(move || cmds.lock().unwrap().push(Cmd::Align(a))).render(ctx);
            x += wd + 4.0;
        }
        let (name, dirty, status, ed) = {
            let d = self.doc.borrow();
            (d.name.clone(), d.dirty, d.status.clone(), d.ed.clone())
        };
        let title = if dirty { format!("{name} (edited)") } else { name };
        ctx.add(Node::Text { rect: Rect::new(x + 12.0, 14.0, (w - x - 24.0).max(0.0), 22.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Rect { rect: Rect::new(0.0, TOOLBAR_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });

        // The pages.
        RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), Self::pages_rect(w, h)).emit(ctx);

        // Status line: page, pages (≈ while pagination runs), and the last message.
        let sy = h - STATUS_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        let line = {
            let mut e = ed.borrow_mut();
            let c = e.caret;
            let page = e.spot(c).page + 1;
            let n = e.pages.pages().len();
            let total = if e.pages.done() { format!("{n}") } else { format!("≈{n}+") };
            let paras = e.doc.blocks.len();
            let pos = format!("Page {page} of {total} · {paras} paragraphs");
            if status.is_empty() {
                pos
            } else {
                format!("{pos} · {status}")
            }
        };
        ctx.add(Node::Text { rect: Rect::new(12.0, sy + 4.0, (w - 24.0).max(0.0), 18.0), content: line, style: st(type_size::XS, Weight::Regular, t.text_secondary()) });

        if self.ui.get_cloned().asking {
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICKER_SLOT);
        }
        ctx.pop();
    }
}
