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
use officina_spell::{Dictionary, Speller};
use officina_text::find::Query;
use pergola_richtext::{Checker, Editor};
use pergola::text_edit::TextEdit;
use pergola::widgets::{Button, MenuButton, MenuItem, MenuState, ScrollState, TextField};
use pergola::{Color, Mutable, TextStyle};
use pergola_richtext::editor::Toggle;
use pergola_richtext::RichTextView;

use crate::doc::Doc;

pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const STATUS_H: f32 = 26.0;
/// The find bar, under the toolbar while it is open.
pub const FIND_H: f32 = 48.0;

/// The paragraph styles the Style menu offers, in order.
pub const STYLES: [&str; 5] = ["Normal", "Title", "Heading 1", "Heading 2", "Heading 3"];

/// The Tools menu, in order.
pub const TOOLS: [&str; 2] = ["Find and Replace", "Spelling"];

/// The Insert menu, in order.
pub const INSERT: [&str; 7] = ["Picture…", "Table", "Row Below", "Column Right", "Delete Row", "Delete Column", "Page Numbers"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Open,
    Save,
    SaveAs,
    ExportPdf,
    Undo,
    Redo,
    Toggle(Toggle),
    Align(Align),
    Style(usize),
    List(ListKind),
    PageNumbers,
    InsertPicture,
    Table,
    RowBelow,
    ColumnRight,
    DeleteRow,
    DeleteColumn,
    FindOpen,
    FindClose,
    FindNext,
    FindPrevious,
    Replace,
    ReplaceAll,
    MatchCase,
    WholeWord,
    SpellOpen,
    SpellClose,
    SpellNext,
    SpellUse(usize),
    SpellIgnore,
    SpellAdd,
}

impl Cmd {
    /// The command of Insert menu item `i`.
    pub fn insert(i: usize) -> Cmd {
        [Cmd::InsertPicture, Cmd::Table, Cmd::RowBelow, Cmd::ColumnRight, Cmd::DeleteRow, Cmd::DeleteColumn, Cmd::PageNumbers][i.min(INSERT.len() - 1)]
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ui {
    pub asking: bool,
}

/// The find bar's state (its two fields are [`Write::find_text`] and
/// [`Write::replace_text`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FindBar {
    pub open: bool,
    pub match_case: bool,
    pub whole_word: bool,
    /// What the last find or replace found.
    pub message: String,
}

/// The spelling bar's state: the word selected and what to offer for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpellBar {
    pub open: bool,
    pub word: String,
    pub suggestions: Vec<String>,
    pub message: String,
}

/// The editor's view of the app's speller.
pub struct AppChecker(pub Rc<RefCell<Speller>>);

impl Checker for AppChecker {
    fn misspelled(&self, text: &str) -> Vec<(u32, u32)> {
        self.0.borrow().misspelled(text)
    }
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
    pub insert_menu: Mutable<MenuState>,
    pub align_menu: Mutable<MenuState>,
    pub find: Mutable<FindBar>,
    pub find_text: Mutable<TextEdit>,
    pub replace_text: Mutable<TextEdit>,
    pub tools_menu: Mutable<MenuState>,
    pub spell: Mutable<SpellBar>,
    pub speller: Rc<RefCell<Speller>>,
    /// Where words added to the dictionary are kept (one per line).
    pub words_file: Rc<RefCell<Option<std::path::PathBuf>>>,
}

impl Write {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let w = Self { doc: Rc::new(RefCell::new(doc)), scroll, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), style_menu: Mutable::new(MenuState::default()), insert_menu: Mutable::new(MenuState::default()), align_menu: Mutable::new(MenuState::default()), find: Mutable::new(FindBar::default()), find_text: Mutable::new(TextEdit::new("")), replace_text: Mutable::new(TextEdit::new("")), tools_menu: Mutable::new(MenuState::default()), spell: Mutable::new(SpellBar::default()), speller: Rc::new(RefCell::new(Speller::new(Dictionary::en_us()))), words_file: Rc::new(RefCell::new(None)) };
        w.attach_checker();
        w
    }

    /// Hands the speller to the document's editor.
    pub fn attach_checker(&self) {
        let d = self.doc.borrow();
        d.ed.borrow_mut().checker = Some(Rc::new(AppChecker(self.speller.clone())));
    }

    /// Keeps the person's own words in `path` (read now, appended to on Add).
    pub fn keep_words(&self, path: std::path::PathBuf) {
        if let Ok(t) = std::fs::read_to_string(&path) {
            let mut sp = self.speller.borrow_mut();
            for w in t.lines().map(str::trim).filter(|w| !w.is_empty() && w.len() <= 100).take(100_000) {
                sp.added.insert(w.to_string());
            }
        }
        *self.words_file.borrow_mut() = Some(path);
        self.doc.borrow().ed.borrow_mut().recheck();
    }

    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new document on screen, at its top.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        self.attach_checker();
        let mut st = self.scroll.get();
        st.physics.set_position(0.0);
        self.scroll.set(st);
        self.changed();
        old
    }

    /// Where the pages go: under the toolbar and the find or spelling bar
    /// when one is open.
    pub fn pages_rect(&self, w: f32, h: f32) -> Rect {
        let top = TOOLBAR_H + if self.find.get_cloned().open || self.spell.get_cloned().open { FIND_H } else { 0.0 };
        Rect::new(0.0, top, w, (h - top - STATUS_H).max(40.0))
    }

    fn query(&self) -> Query {
        let f = self.find.get_cloned();
        Query { text: self.find_text.get_cloned().text().to_string(), match_case: f.match_case, whole_word: f.whole_word }
    }

    /// Runs a spelling bar command.
    fn spell_cmd(&self, c: Cmd) {
        let mut sb = self.spell.get_cloned();
        let ed = self.doc.borrow().ed.clone();
        let word = |ed: &Editor| -> String { ed.selected_text() };
        match c {
            Cmd::SpellOpen => {
                sb.open = true;
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
            }
            Cmd::SpellClose => {
                self.spell.set(SpellBar::default());
                return;
            }
            Cmd::SpellUse(i) => {
                if let Some(s) = sb.suggestions.get(i).cloned() {
                    ed.borrow_mut().replace_selection(&s);
                }
            }
            Cmd::SpellIgnore => {
                if !sb.word.is_empty() {
                    self.speller.borrow_mut().ignored.insert(sb.word.clone());
                    ed.borrow_mut().recheck();
                }
            }
            Cmd::SpellAdd => {
                if !sb.word.is_empty() {
                    self.speller.borrow_mut().added.insert(sb.word.clone());
                    if let Some(p) = self.words_file.borrow().as_ref() {
                        use std::io::Write as _;
                        let _ = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| writeln!(f, "{}", sb.word));
                    }
                    ed.borrow_mut().recheck();
                }
            }
            _ => {}
        }
        // On to the next misspelled word.
        let found = ed.borrow_mut().next_misspelled();
        if found {
            let w = word(&ed.borrow());
            sb.suggestions = self.speller.borrow().suggest(&w);
            sb.message = if sb.suggestions.is_empty() { "No suggestions.".into() } else { String::new() };
            sb.word = w;
        } else {
            sb.word.clear();
            sb.suggestions.clear();
            sb.message = "No misspelled words.".into();
        }
        self.spell.set(sb);
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Runs a find bar command on the editor.
    fn find_cmd(&self, c: Cmd) {
        let mut f = self.find.get_cloned();
        match c {
            Cmd::FindOpen => {
                f.open = true;
                self.spell.set(SpellBar::default());
                // The selection, when it is on one line, is what to find.
                let sel = self.doc.borrow().ed.borrow().selected_text();
                if !sel.is_empty() && !sel.contains('\n') && sel.chars().count() <= 200 {
                    let mut t = TextEdit::new(&sel);
                    t.select_all();
                    self.find_text.set(t);
                }
                f.message.clear();
            }
            Cmd::FindClose => f = FindBar { open: false, ..f },
            Cmd::MatchCase => f.match_case = !f.match_case,
            Cmd::WholeWord => f.whole_word = !f.whole_word,
            _ => {}
        }
        self.find.set(f.clone());
        if !matches!(c, Cmd::FindNext | Cmd::FindPrevious | Cmd::Replace | Cmd::ReplaceAll) {
            return;
        }
        let q = self.query();
        if q.text.is_empty() {
            return;
        }
        let with = self.replace_text.get_cloned().text().to_string();
        let msg = {
            let d = self.doc.borrow();
            let mut e = d.ed.borrow_mut();
            match c {
                Cmd::ReplaceAll => match e.replace_all(&q, &with) {
                    0 => "No matches.".to_string(),
                    1 => "Replaced 1 match.".to_string(),
                    n => format!("Replaced {n} matches."),
                },
                _ => {
                    let found = match c {
                        Cmd::Replace => {
                            e.replace(&q, &with);
                            e.has_selection()
                        }
                        _ => e.find_next(&q, c == Cmd::FindNext),
                    };
                    if found { String::new() } else { "No matches.".to_string() }
                }
            }
        };
        self.find.set(FindBar { message: msg, ..self.find.get_cloned() });
        let (w, h) = self.size.get();
        let ed = self.doc.borrow().ed.clone();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
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
                    Cmd::Table => {
                        e.insert_table(3, 3);
                    }
                    Cmd::RowBelow => {
                        e.insert_row_below();
                    }
                    Cmd::ColumnRight => {
                        e.insert_column_right();
                    }
                    Cmd::DeleteRow => {
                        e.delete_row();
                    }
                    Cmd::DeleteColumn => {
                        e.delete_column();
                    }
                    Cmd::SpellOpen | Cmd::SpellClose | Cmd::SpellNext | Cmd::SpellUse(_) | Cmd::SpellIgnore | Cmd::SpellAdd => {
                        drop(e);
                        drop(d);
                        self.spell_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::FindOpen | Cmd::FindClose | Cmd::FindNext | Cmd::FindPrevious | Cmd::Replace | Cmd::ReplaceAll | Cmd::MatchCase | Cmd::WholeWord => {
                        drop(e);
                        drop(d);
                        self.find_cmd(c);
                        self.changed();
                        return false;
                    }
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

    /// Places a picture from its encoded bytes at the caret.
    pub fn insert_picture(&self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.len() > navigator_image::image::MAX_ENCODED_IMAGE_BYTES {
            return Err("it is over 16 MB".into());
        }
        let (_, w, h) = navigator_image::image::natural_size(&bytes).map_err(|k| format!("not a picture this app can show ({k})"))?;
        let d = self.doc.borrow();
        d.ed.borrow_mut().insert_image(officina_text::model::ImageData { bytes: bytes.into(), width: w, height: h });
        drop(d);
        self.sync();
        Ok(())
    }

    /// Journals the editor's actions; marks the document edited.
    pub fn sync(&self) {
        if self.doc.borrow_mut().sync_log() {
            self.changed();
        }
    }
}

/// 1234567 as "1,234,567".
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
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
        button(ctx, "Save As…", Cmd::SaveAs, 88.0, 6.0);
        button(ctx, "PDF…", Cmd::ExportPdf, 60.0, 10.0);
        button(ctx, "Undo", Cmd::Undo, 60.0, 6.0);
        button(ctx, "Redo", Cmd::Redo, 60.0, 10.0);
        button(ctx, "B", Cmd::Toggle(Toggle::Bold), 36.0, 4.0);
        button(ctx, "I", Cmd::Toggle(Toggle::Italic), 36.0, 4.0);
        button(ctx, "U", Cmd::Toggle(Toggle::Underline), 36.0, 10.0);
        button(ctx, "• List", Cmd::List(ListKind::Bullet), 64.0, 4.0);
        button(ctx, "1. List", Cmd::List(ListKind::Numbered), 68.0, 10.0);
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
            x += 128.0;
        }
        {
            let in_table = self.doc.borrow().ed.borrow().doc.cell(self.doc.borrow().ed.borrow().caret.block).is_some();
            let items: Vec<MenuItem> = INSERT.iter().enumerate().map(|(i, s)| MenuItem::new(*s).enabled(!(2..=5).contains(&i) || in_table)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Insert".to_string(), self.insert_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::insert(i))).at(x, 8.0).width(96.0).render(ctx);
            x += 100.0;
        }
        {
            let items: Vec<MenuItem> = TOOLS.iter().map(|s| MenuItem::new(*s)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Tools".to_string(), self.tools_menu.clone(), items, move |i| cmds.lock().unwrap().push(if i == 0 { Cmd::FindOpen } else { Cmd::SpellOpen })).at(x, 8.0).width(88.0).render(ctx);
            x += 104.0;
        }
        {
            // Alignment, as a menu (the toolbar has no room for four buttons).
            let all = [("Left", Align::Start), ("Centre", Align::Center), ("Right", Align::End), ("Justify", Align::Justify)];
            let items: Vec<MenuItem> = all.iter().map(|(l, a)| MenuItem::new(*l).radio(*a == cur_align)).collect();
            let now = all.iter().find(|(_, a)| *a == cur_align).map_or("Left", |(l, _)| l);
            let cmds = self.cmds.clone();
            MenuButton::new(now.to_string(), self.align_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Align(all[i.min(3)].1))).at(x, 8.0).width(100.0).render(ctx);
            x += 108.0;
        }
        let (name, dirty, status, ed) = {
            let d = self.doc.borrow();
            (d.name.clone(), d.dirty, d.status.clone(), d.ed.clone())
        };
        let title = if dirty { format!("{name} (edited)") } else { name };
        ctx.add(Node::Text { rect: Rect::new(x + 12.0, 14.0, (w - x - 24.0).max(0.0), 22.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Rect { rect: Rect::new(0.0, TOOLBAR_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });

        // The find bar.
        let fb = self.find.get_cloned();
        if fb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let fw = ((w - 640.0) / 2.0).clamp(140.0, 260.0);
            let cmds = self.cmds.clone();
            TextField::new(self.find_text.clone()).placeholder("Find").autofocus(true).at(12.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::FindNext)).render(ctx);
            let mut x = 12.0 + fw + 6.0;
            Button::secondary("‹").at(x, y + 8.0).width(32.0).on_click(push(Cmd::FindPrevious)).render(ctx);
            x += 36.0;
            Button::secondary("›").at(x, y + 8.0).width(32.0).on_click(push(Cmd::FindNext)).render(ctx);
            x += 36.0;
            let toggle = |on: bool, label: &str| if on { Button::primary(label) } else { Button::secondary(label) };
            toggle(fb.match_case, "Aa").at(x, y + 8.0).width(40.0).on_click(push(Cmd::MatchCase)).render(ctx);
            x += 44.0;
            toggle(fb.whole_word, "Word").at(x, y + 8.0).width(56.0).on_click(push(Cmd::WholeWord)).render(ctx);
            x += 72.0;
            let cmds = self.cmds.clone();
            TextField::new(self.replace_text.clone()).placeholder("Replace with").at(x, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Replace)).render(ctx);
            x += fw + 6.0;
            Button::secondary("Replace").at(x, y + 8.0).width(76.0).on_click(push(Cmd::Replace)).render(ctx);
            x += 80.0;
            Button::secondary("All").at(x, y + 8.0).width(48.0).on_click(push(Cmd::ReplaceAll)).render(ctx);
            x += 60.0;
            if !fb.message.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, (w - x - 56.0).max(0.0), 18.0), content: fb.message.clone(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            }
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::FindClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The spelling bar.
        let sb = self.spell.get_cloned();
        if sb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let mut x = 12.0;
            let (label, color) = if sb.word.is_empty() { (sb.message.clone(), t.text_secondary()) } else { (format!("“{}”", sb.word), Color::from_srgb8(0xb0, 0x1c, 0x1c)) };
            ctx.add(Node::Text { rect: Rect::new(x, y + 14.0, 180.0, 20.0), content: label, style: st(type_size::MD, Weight::Semibold, color) });
            x += 188.0;
            if !sb.word.is_empty() {
                for (i, s) in sb.suggestions.iter().take(5).enumerate() {
                    let wd = (Node::measure_text(s, &st(type_size::MD, Weight::Regular, t.text_primary())).w + 28.0).clamp(48.0, 180.0);
                    if x + wd > w - 308.0 {
                        break;
                    }
                    let b = if i == 0 { Button::primary(s.as_str()) } else { Button::secondary(s.as_str()) };
                    b.at(x, y + 8.0).width(wd).on_click(push(Cmd::SpellUse(i))).render(ctx);
                    x += wd + 4.0;
                }
                if !sb.message.is_empty() {
                    ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, 140.0, 18.0), content: sb.message.clone(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                }
                let bx = w - 300.0;
                Button::secondary("Ignore").at(bx, y + 8.0).width(72.0).on_click(push(Cmd::SpellIgnore)).render(ctx);
                Button::secondary("Add").at(bx + 76.0, y + 8.0).width(60.0).on_click(push(Cmd::SpellAdd)).render(ctx);
                Button::secondary("Next").at(bx + 140.0, y + 8.0).width(64.0).on_click(push(Cmd::SpellNext)).render(ctx);
            }
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::SpellClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The pages.
        let cmds = self.cmds.clone();
        RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).on_find(move || cmds.lock().unwrap().push(Cmd::FindOpen)).emit(ctx);

        // Status line: page, pages (≈ while pagination runs), and the last message.
        let sy = h - STATUS_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        let line = {
            let mut e = ed.borrow_mut();
            let c = e.caret;
            let page = e.spot(c).page + 1;
            let n = e.pages.pages().len();
            let total = if e.pages.done() { format!("{n}") } else { format!("≈{n}+") };
            let words = if e.has_selection() {
                format!("{} of {} words", group(e.counts().words), group(e.words.total().words))
            } else {
                let c = e.words.total();
                format!("{} words, {} characters", group(c.words), group(c.chars))
            };
            let pos = format!("Page {page} of {total} · {words}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_bar_commands() {
        let doc = Doc::from_text("t.txt", b"one cat\ntwo cats\nCat three").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let sel = || {
            let d = w.doc.borrow();
            let e = d.ed.borrow();
            (e.anchor.block, e.anchor.at, e.caret.at)
        };
        run(Cmd::FindOpen);
        assert!(w.find.get_cloned().open);
        assert!(w.pages_rect(1200.0, 800.0).y() > TOOLBAR_H);
        w.find_text.set(TextEdit::new("cat"));
        run(Cmd::FindNext);
        assert_eq!(sel(), (0, 4, 7));
        run(Cmd::WholeWord);
        run(Cmd::FindNext);
        assert_eq!(sel(), (2, 0, 3));
        run(Cmd::MatchCase);
        run(Cmd::FindNext);
        assert_eq!(sel(), (0, 4, 7));
        run(Cmd::MatchCase);
        run(Cmd::WholeWord);
        w.replace_text.set(TextEdit::new("dog"));
        run(Cmd::ReplaceAll);
        assert_eq!(w.find.get_cloned().message, "Replaced 3 matches.");
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "one dog\ntwo dogs\ndog three");
        run(Cmd::Undo);
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "one cat\ntwo cats\nCat three");
        w.find_text.set(TextEdit::new("zebra"));
        run(Cmd::FindNext);
        assert_eq!(w.find.get_cloned().message, "No matches.");
        run(Cmd::FindClose);
        assert_eq!(w.pages_rect(1200.0, 800.0).y(), TOOLBAR_H);
        assert_eq!(group(1234567), "1,234,567");
        // Spelling: the bar finds the misspelled word, offers the fix, applies it.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        let doc = Doc::from_text("s.txt", b"A wrod here and teh end.").unwrap();
        w.replace(doc);
        run(Cmd::SpellOpen);
        let sb = w.spell.get_cloned();
        assert_eq!(sb.word, "wrod");
        assert!(sb.suggestions.contains(&"word".to_string()), "{:?}", sb.suggestions);
        let i = sb.suggestions.iter().position(|s| s == "word").unwrap();
        run(Cmd::SpellUse(i));
        assert_eq!(w.spell.get_cloned().word, "teh");
        run(Cmd::SpellIgnore);
        assert_eq!(w.spell.get_cloned().message, "No misspelled words.");
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "A word here and teh end.");
        run(Cmd::FindOpen);
        assert!(!w.spell.get_cloned().open);
        assert_eq!(group(12), "12");
    }
}
