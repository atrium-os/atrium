//! The window's contents: a toolbar, the pages (Pergola's `RichTextEditor`), a
//! status line.
//!
//! The view owns no I/O: buttons queue a [`Cmd`]; file commands go to the event
//! loop (the binary, which talks to the picker), editing commands run in
//! [`Write::take_cmds`]. The same view runs headless in tests and examples.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use officina_text::model::{HAlign, Rel, Wrap, SectionStart, Align, CommentId, ListKind, NoteId, NoteKind};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use officina_spell::{Dictionary, Speller};
use officina_text::find::Query;
use officina_text::merge::{MergeError, Take};
use officina_text::track::EditKind;
use officina_text::track::Resolve;
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
pub const TOOLS: [&str; 7] = ["Find and Replace", "Spelling", "Track Changes", "Review Changes", "Compare with…", "Merge with…", "Password…"];

/// The Insert menu, in order.
pub const INSERT: [&str; 34] = ["Picture…", "Table", "Row Below", "Column Right", "Delete Row", "Delete Column", "Page Numbers", "Footnote", "Endnote", "Edit Note", "Comment", "Edit Comment", "Table of Contents", "Cross-reference…", "Section Break", "Continuous Section Break", "One Column", "Two Columns", "Three Columns", "Line Between Columns", "In Line with Text", "Wrap Square", "Wrap Top and Bottom", "Behind Text", "In Front of Text", "Picture Left", "Picture Centre", "Picture Right", "Portrait", "Landscape", "A4 Paper", "Letter Paper", "Normal Margins", "Narrow Margins"];

/// Paragraphs of a note as the note bar's one line shows them.
pub const NOTE_PARA_MARK: &str = " ¶ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Tools ▸ Password…: set, change or remove the document's password.
    PasswordOpen,
    /// The password bar's field submitted (to set, or to open a file).
    PasswordDone,
    PasswordClose,
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
    NoteInsert(NoteKind),
    NoteEdit,
    NoteDone,
    NoteClose,
    CommentAdd,
    CommentEdit,
    /// A balloon was clicked.
    CommentOpen(CommentId),
    CommentDone,
    CommentReply,
    CommentResolve,
    CommentDelete,
    CommentClose,
    /// Tracking on or off.
    TrackToggle,
    ReviewOpen,
    ReviewClose,
    ReviewPrevious,
    ReviewNext,
    ReviewAccept,
    ReviewReject,
    ReviewAcceptAll,
    ReviewRejectAll,
    /// Compare with another version (the app asks for the file).
    CompareWith,
    /// Merge another reviewed copy in (the app asks for the file).
    MergeWith,
    /// Settle the picked conflict.
    ReviewTake(Take),
    /// Insert the table of contents, or bring it up to date.
    Toc,
    XrefOpen,
    XrefClose,
    /// The previous or next heading in the cross-reference bar.
    XrefMove(bool),
    /// Insert a reference to the bar's heading: its page (true) or its text.
    XrefInsert(bool),
    /// A section break at the caret.
    SectionBreak(SectionStart),
    /// The caret's section in this many columns.
    Columns(u8),
    /// A line between the caret's section's columns, on or off.
    ColumnRule,
    /// The picture at the caret floats with this wrap (none: in line).
    Wrap(Option<Wrap>),
    /// The floating picture at the caret to its column's start, centre or end.
    FloatAlign(HAlign),
    /// The caret's section's page: orientation, paper or margins.
    Page(PageCmd),
}

/// A change to a section's page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCmd {
    Portrait,
    Landscape,
    A4,
    Letter,
    /// Margins all round, twips (1440: normal; 720: narrow).
    Margins(i32),
}

impl PageCmd {
    /// `g` with this change: paper keeps the orientation, margins all round.
    pub fn apply(self, g: officina_text::model::PageSetup) -> officina_text::model::PageSetup {
        let land = g.width > g.height;
        let paper = |w: i32, h: i32| if land { (h, w) } else { (w, h) };
        let mut n = g;
        match self {
            PageCmd::Portrait if land => (n.width, n.height) = (g.height, g.width),
            PageCmd::Landscape if !land => (n.width, n.height) = (g.height, g.width),
            PageCmd::A4 => (n.width, n.height) = paper(11906, 16838),
            PageCmd::Letter => (n.width, n.height) = paper(12240, 15840),
            PageCmd::Margins(m) => (n.margin_top, n.margin_bottom, n.margin_start, n.margin_end) = (m, m, m, m),
            _ => {}
        }
        n
    }

    /// Whether `g` is already so.
    pub fn is(self, g: &officina_text::model::PageSetup) -> bool {
        let (a, b) = (g.width.min(g.height), g.width.max(g.height));
        match self {
            PageCmd::Portrait => g.width <= g.height,
            PageCmd::Landscape => g.width > g.height,
            PageCmd::A4 => (a - 11906).abs() <= 20 && (b - 16838).abs() <= 20,
            PageCmd::Letter => (a - 12240).abs() <= 20 && (b - 15840).abs() <= 20,
            PageCmd::Margins(m) => [g.margin_top, g.margin_bottom, g.margin_start, g.margin_end].iter().all(|&x| x == m),
        }
    }
}

/// The page commands of Insert menu items 28 on.
pub const PAGE_CMDS: [PageCmd; 6] = [PageCmd::Portrait, PageCmd::Landscape, PageCmd::A4, PageCmd::Letter, PageCmd::Margins(1440), PageCmd::Margins(720)];

impl Cmd {
    /// The command of Insert menu item `i`.
    pub fn insert(i: usize) -> Cmd {
        [Cmd::InsertPicture, Cmd::Table, Cmd::RowBelow, Cmd::ColumnRight, Cmd::DeleteRow, Cmd::DeleteColumn, Cmd::PageNumbers, Cmd::NoteInsert(NoteKind::Foot), Cmd::NoteInsert(NoteKind::End), Cmd::NoteEdit, Cmd::CommentAdd, Cmd::CommentEdit, Cmd::Toc, Cmd::XrefOpen, Cmd::SectionBreak(SectionStart::NextPage), Cmd::SectionBreak(SectionStart::Continuous), Cmd::Columns(1), Cmd::Columns(2), Cmd::Columns(3), Cmd::ColumnRule, Cmd::Wrap(None), Cmd::Wrap(Some(Wrap::Square)), Cmd::Wrap(Some(Wrap::TopBottom)), Cmd::Wrap(Some(Wrap::Behind)), Cmd::Wrap(Some(Wrap::InFront)), Cmd::FloatAlign(HAlign::Start), Cmd::FloatAlign(HAlign::Center), Cmd::FloatAlign(HAlign::End), Cmd::Page(PAGE_CMDS[0]), Cmd::Page(PAGE_CMDS[1]), Cmd::Page(PAGE_CMDS[2]), Cmd::Page(PAGE_CMDS[3]), Cmd::Page(PAGE_CMDS[4]), Cmd::Page(PAGE_CMDS[5])][i.min(INSERT.len() - 1)]
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

/// The password bar: asking for a protected file's password, or for the
/// document's new one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PasswordBar {
    pub open: bool,
    pub prompt: String,
    /// Setting the document's password (else opening a file).
    pub setting: bool,
}

/// The note bar: which note's text its field holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NoteBar {
    pub open: bool,
    pub id: NoteId,
    /// "Footnote 3", "Endnote ii".
    pub title: String,
}

/// The comment bar: the thread it is on, and whether its field holds the
/// first comment's text or a new reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommentBar {
    pub open: bool,
    /// The thread's first comment.
    pub id: CommentId,
    pub replying: bool,
    /// "Comment · Ann · 2 replies".
    pub title: String,
    pub done: bool,
}

/// The cross-reference bar: which heading it points at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XrefBar {
    pub open: bool,
    /// Index into the editor's [`Editor::ref_targets`].
    pub at: usize,
}

/// `s` cut with an ellipsis to fit `room` wide in `style`.
pub fn fit_text(s: &str, room: f32, style: &pergola::node::TextStyle) -> String {
    if Node::measure_text(s, style).w <= room {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() && Node::measure_text(&format!("{}…", chars.iter().collect::<String>()), style).w > room {
        chars.truncate(chars.len().saturating_sub(4.max(chars.len() / 16)));
    }
    format!("{}…", chars.iter().collect::<String>().trim_end())
}

/// Who the app writes comments as: `$USER` (else "Me") and its initials.
pub fn comment_author() -> (String, String) {
    let name = std::env::var("USER").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| "Me".into());
    let initials: String = name.split(|c: char| c.is_whitespace() || c == '.' || c == '_').filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).take(3).collect();
    (name, initials)
}

/// Now, as a comment date.
pub fn comment_date() -> String {
    officina_text::model::iso_utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
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
    pub xref: Mutable<XrefBar>,
    pub password: Mutable<PasswordBar>,
    pub password_text: Mutable<TextEdit>,
    pub note: Mutable<NoteBar>,
    pub note_text: Mutable<TextEdit>,
    pub comment: Mutable<CommentBar>,
    pub comment_text: Mutable<TextEdit>,
    pub speller: Rc<RefCell<Speller>>,
    /// Where words added to the dictionary are kept (one per line).
    pub words_file: Rc<RefCell<Option<std::path::PathBuf>>>,
}

impl Write {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let w = Self { doc: Rc::new(RefCell::new(doc)), scroll, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), style_menu: Mutable::new(MenuState::default()), insert_menu: Mutable::new(MenuState::default()), align_menu: Mutable::new(MenuState::default()), find: Mutable::new(FindBar::default()), find_text: Mutable::new(TextEdit::new("")), replace_text: Mutable::new(TextEdit::new("")), tools_menu: Mutable::new(MenuState::default()), spell: Mutable::new(SpellBar::default()), xref: Mutable::new(XrefBar::default()), password: Mutable::new(PasswordBar::default()), password_text: Mutable::new(TextEdit::new("").secret(true)), note: Mutable::new(NoteBar::default()), note_text: Mutable::new(TextEdit::new("")), comment: Mutable::new(CommentBar::default()), comment_text: Mutable::new(TextEdit::new("")), speller: Rc::new(RefCell::new(Speller::new(Dictionary::en_us()))), words_file: Rc::new(RefCell::new(None)) };
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
        let review = self.doc.borrow().ed.borrow().review;
        let top = TOOLBAR_H + if review || self.find.get_cloned().open || self.spell.get_cloned().open || self.note.get_cloned().open || self.comment.get_cloned().open || self.xref.get_cloned().open { FIND_H } else { 0.0 };
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
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
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

    /// Runs a note command: insert a note and open the bar on it, open the bar
    /// on the note beside the caret, or put the bar's text in the note.
    fn note_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        let open_on = |id: NoteId| {
            let e = ed.borrow();
            let text = e.note_text(id).unwrap_or_default().replace('\n', NOTE_PARA_MARK);
            let title = match e.doc.note_numbers().get(&id) {
                Some(&(NoteKind::Foot, n)) => format!("Footnote {}", officina_text::model::note_label(NoteKind::Foot, n)),
                Some(&(NoteKind::End, n)) => format!("Endnote {}", officina_text::model::note_label(NoteKind::End, n)),
                None => "Note".into(),
            };
            let mut t = TextEdit::new(&text);
            t.select_all();
            self.note_text.set(t);
            self.note.set(NoteBar { open: true, id, title });
            self.comment.set(CommentBar::default());
            self.find.set(FindBar { open: false, ..self.find.get_cloned() });
            self.spell.set(SpellBar::default());
        };
        match c {
            Cmd::NoteInsert(kind) => {
                let id = ed.borrow_mut().insert_note(kind, "");
                if let Some(id) = id {
                    open_on(id);
                }
            }
            Cmd::NoteEdit => {
                let id = ed.borrow().note_at_caret();
                if let Some(id) = id {
                    open_on(id);
                }
            }
            Cmd::NoteDone => {
                let nb = self.note.get_cloned();
                if nb.open {
                    let text = self.note_text.get_cloned().text().split('¶').map(str::trim).collect::<Vec<_>>().join("\n");
                    ed.borrow_mut().set_note_text(nb.id, &text);
                }
                self.note.set(NoteBar::default());
            }
            Cmd::NoteClose => self.note.set(NoteBar::default()),
            _ => {}
        }
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Opens the comment bar on thread `root`: on its first comment's text,
    /// or on a new reply.
    fn open_comment(&self, root: CommentId, replying: bool) {
        let ed = self.doc.borrow().ed.clone();
        let e = ed.borrow();
        let Some(c) = e.doc.comments.get(root).cloned() else {
            self.comment.set(CommentBar::default());
            return;
        };
        let n = e.thread(root).len() - 1;
        let mut title = if c.author.is_empty() { "Comment".to_string() } else { format!("Comment · {}", c.author) };
        if n > 0 {
            title += &format!(" · {n} {}", if n == 1 { "reply" } else { "replies" });
        }
        let shown = if replying { String::new() } else { c.text.replace('\n', NOTE_PARA_MARK) };
        let mut t = TextEdit::new(&shown);
        t.select_all();
        drop(e);
        self.comment_text.set(t);
        self.comment.set(CommentBar { open: true, id: root, replying, title, done: c.done });
        self.note.set(NoteBar::default());
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
    }

    /// Runs a comment command.
    fn comment_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        let cb = self.comment.get_cloned();
        let field = || self.comment_text.get_cloned().text().split('¶').map(str::trim).collect::<Vec<_>>().join("\n");
        match c {
            Cmd::CommentAdd => {
                let (name, initials) = comment_author();
                let id = ed.borrow_mut().add_comment("", &name, &initials, &comment_date());
                if let Some(id) = id {
                    self.open_comment(id, false);
                }
            }
            Cmd::CommentEdit => {
                let id = ed.borrow().comment_at_caret();
                if let Some(id) = id {
                    self.open_comment(id, false);
                }
            }
            Cmd::CommentOpen(id) => {
                let root = ed.borrow().thread_root(id);
                let start = ed.borrow().doc.comment_ranges().into_iter().find(|r| r.0 == root).map(|r| r.1);
                if let Some((b, at)) = start {
                    ed.borrow_mut().set_caret(pergola_richtext::Pos::new(b, at), false);
                }
                self.open_comment(root, false);
            }
            Cmd::CommentDone if cb.open => {
                let text = field();
                if cb.replying {
                    if !text.is_empty() {
                        let (name, initials) = comment_author();
                        ed.borrow_mut().reply_comment(cb.id, &text, &name, &initials, &comment_date());
                    }
                } else if text.is_empty() && ed.borrow().thread(cb.id).len() == 1 {
                    // A comment left empty goes away.
                    ed.borrow_mut().delete_comment(cb.id);
                } else {
                    ed.borrow_mut().edit_comment(cb.id, Some(&text), None);
                }
                self.comment.set(CommentBar::default());
            }
            Cmd::CommentReply if cb.open => {
                // Keep what was typed into the first comment, then start a reply.
                if !cb.replying {
                    let text = field();
                    if !text.is_empty() {
                        ed.borrow_mut().edit_comment(cb.id, Some(&text), None);
                    }
                }
                self.open_comment(cb.id, true);
            }
            Cmd::CommentResolve if cb.open => {
                ed.borrow_mut().edit_comment(cb.id, None, Some(!cb.done));
                self.open_comment(cb.id, cb.replying);
            }
            Cmd::CommentDelete if cb.open => {
                ed.borrow_mut().delete_comment(cb.id);
                self.comment.set(CommentBar::default());
            }
            Cmd::CommentClose => self.comment.set(CommentBar::default()),
            _ => {}
        }
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Compares `original` (an earlier version, named `name`) with the
    /// document: what differs becomes tracked changes, shown in review.
    pub fn compare_with(&self, original: &Doc, name: &str) {
        let ed = self.doc.borrow().ed.clone();
        let orig = original.ed.borrow().doc.clone();
        let n = ed.borrow_mut().compare_with(&orig);
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
        self.note.set(NoteBar::default());
        self.comment.set(CommentBar::default());
        let msg = match n {
            None => "This document has tracked changes: accept or reject them before comparing.".to_string(),
            Some(0) => format!("No differences from “{name}”."),
            Some(1) => format!("Compared with “{name}”: 1 edit."),
            Some(n) => format!("Compared with “{name}”: {n} edits."),
        };
        self.doc.borrow_mut().status = msg;
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
        self.changed();
    }

    /// Merges `other` (another reviewed copy, named `name`) into the
    /// document: its changes come in under their authors, conflicts wait in
    /// review.
    pub fn merge_with(&self, other: &Doc, name: &str) {
        let ed = self.doc.borrow().ed.clone();
        let theirs = other.ed.borrow().doc.clone();
        let who = name.rsplit_once('.').map_or(name, |(s, _)| s).to_string();
        let r = ed.borrow_mut().merge_with(&theirs, &who);
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
        self.note.set(NoteBar::default());
        self.comment.set(CommentBar::default());
        let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let msg = match r {
            Err(MergeError::NotSameOriginal) => format!("“{name}” isn't a copy of this document's original."),
            Err(MergeError::HasConflicts) => format!("“{name}” has conflicts of its own: settle them there first."),
            Ok(m) if m.applied == 0 && m.conflicts == 0 => format!("Nothing new in “{name}”."),
            Ok(m) if m.conflicts == 0 => format!("Merged “{name}”: {} added.", plural(m.applied, "change", "changes")),
            Ok(m) => format!("Merged “{name}”: {} added, {} to settle.", plural(m.applied, "change", "changes"), plural(m.conflicts, "conflict", "conflicts")),
        };
        self.doc.borrow_mut().status = msg;
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
        self.changed();
    }

    /// Runs a find bar command on the editor.
    fn find_cmd(&self, c: Cmd) {
        let mut f = self.find.get_cloned();
        match c {
            Cmd::FindOpen => {
                f.open = true;
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
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

    /// Shows the password bar with `prompt`, its field empty.
    pub fn ask_password(&self, prompt: &str, setting: bool) {
        self.password_text.set(TextEdit::new("").secret(true));
        self.password.set(PasswordBar { open: true, prompt: prompt.into(), setting });
        self.changed();
    }

    /// The password bar's text, and the bar closed and emptied.
    pub fn take_password(&self) -> (PasswordBar, String) {
        let bar = self.password.get_cloned();
        let text = self.password_text.get_cloned().text().to_string();
        self.password.set(PasswordBar::default());
        self.password_text.set(TextEdit::new("").secret(true));
        self.changed();
        (bar, text)
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
                // The bars share one place under the toolbar.
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::XrefOpen) {
                    e.review = false;
                }
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen) {
                    self.xref.set(XrefBar::default());
                }
                match c {
                    Cmd::TrackToggle => {
                        let on = !e.doc.tracking;
                        e.set_tracking(on);
                    }
                    Cmd::ReviewOpen => {
                        drop(e);
                        drop(d);
                        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                        self.spell.set(SpellBar::default());
                        self.note.set(NoteBar::default());
                        self.comment.set(CommentBar::default());
                        let ed = self.doc.borrow().ed.clone();
                        let mut e = ed.borrow_mut();
                        e.review = true;
                        // Start at the edit at or after the caret.
                        let c = (e.caret.block, e.caret.at);
                        let i = e.edits().iter().position(|x| x.ranges[0].1 >= c).unwrap_or(0);
                        e.review_select(i);
                        drop(e);
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::ReviewClose => e.review = false,
                    Cmd::ReviewPrevious | Cmd::ReviewNext | Cmd::ReviewAccept | Cmd::ReviewReject | Cmd::ReviewAcceptAll | Cmd::ReviewRejectAll | Cmd::ReviewTake(_) => {
                        let at = e.review_at;
                        match c {
                            Cmd::ReviewTake(how) => {
                                e.review_take(how);
                            }
                            Cmd::ReviewPrevious => e.review_select(at.saturating_sub(1)),
                            Cmd::ReviewNext => e.review_select(at + 1),
                            Cmd::ReviewAccept => {
                                e.review_resolve(Resolve::Accept);
                            }
                            Cmd::ReviewReject => {
                                e.review_resolve(Resolve::Reject);
                            }
                            Cmd::ReviewAcceptAll => {
                                e.resolve(Resolve::Accept, |_| true);
                            }
                            _ => {
                                e.resolve(Resolve::Reject, |_| true);
                            }
                        }
                        drop(e);
                        drop(d);
                        let ed = self.doc.borrow().ed.clone();
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
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
                    Cmd::SectionBreak(start) => {
                        e.insert_section_break(start);
                    }
                    Cmd::Columns(n) => {
                        let rule = e.section().rule;
                        e.set_columns(n, rule);
                    }
                    Cmd::ColumnRule => {
                        let s = e.section();
                        e.set_columns(s.columns, !s.rule);
                    }
                    Cmd::Wrap(w) => {
                        e.set_wrap(w);
                    }
                    Cmd::Page(c) => {
                        let label = INSERT[28 + PAGE_CMDS.iter().position(|x| *x == c).unwrap_or(0)];
                        e.set_section_page(label, |g| c.apply(g));
                    }
                    Cmd::FloatAlign(a) => {
                        e.set_float_align(a);
                    }
                    Cmd::Toc => {
                        if !e.insert_toc() {
                            drop(e);
                            drop(d);
                            let had = officina_text::fields::toc_range(&self.doc.borrow().ed.borrow().doc, None).is_some();
                            self.doc.borrow_mut().status = if had { "The table of contents is up to date".into() } else { "No headings to list: use the Heading styles".into() };
                            self.changed();
                            return false;
                        }
                    }
                    Cmd::XrefOpen | Cmd::XrefClose | Cmd::XrefMove(_) | Cmd::XrefInsert(_) => {
                        let n = e.ref_targets().len();
                        let mut xb = self.xref.get_cloned();
                        match c {
                            Cmd::XrefOpen => {
                                drop(e);
                                drop(d);
                                if n == 0 {
                                    self.doc.borrow_mut().status = "No headings to refer to: use the Heading styles".into();
                                    self.changed();
                                    return false;
                                }
                                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                                self.spell.set(SpellBar::default());
                                self.note.set(NoteBar::default());
                                self.comment.set(CommentBar::default());
                                self.xref.set(XrefBar { open: true, at: xb.at.min(n - 1) });
                                self.changed();
                                return false;
                            }
                            Cmd::XrefClose => xb = XrefBar::default(),
                            Cmd::XrefMove(fwd) if n > 0 => xb.at = if fwd { (xb.at + 1) % n } else { (xb.at + n - 1) % n },
                            Cmd::XrefInsert(page) => {
                                if let Some(t) = e.ref_targets().get(xb.at) {
                                    let f = if page { officina_text::model::Field::PageRef(t.0) } else { officina_text::model::Field::TextRef(t.0) };
                                    e.insert_field(f);
                                }
                            }
                            _ => {}
                        }
                        self.xref.set(xb);
                    }
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
                    Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::CommentDone | Cmd::CommentReply | Cmd::CommentResolve | Cmd::CommentDelete | Cmd::CommentClose => {
                        drop(e);
                        drop(d);
                        self.comment_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::PasswordOpen => {
                        let protected = d.protection.is_some();
                        drop(e);
                        drop(d);
                        self.ask_password(if protected { "New password (empty removes it)" } else { "Password to protect this document" }, true);
                        return false;
                    }
                    Cmd::PasswordClose => {
                        drop(e);
                        drop(d);
                        self.password.set(PasswordBar::default());
                        self.password_text.set(TextEdit::new("").secret(true));
                        self.changed();
                        return true;
                    }
                    Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::NoteDone | Cmd::NoteClose => {
                        drop(e);
                        drop(d);
                        self.note_cmd(c);
                        self.changed();
                        return false;
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
            let on_note = self.doc.borrow().ed.borrow().note_at_caret().is_some();
            let has_toc = officina_text::fields::toc_range(&self.doc.borrow().ed.borrow().doc, None).is_some();
            let sec = self.doc.borrow().ed.borrow().section();
            let page = self.doc.borrow().ed.borrow().section_page();
            let pic = self.doc.borrow().ed.borrow().picture_at_caret().map(|x| x.2.float);
            let wraps = [None, Some(Wrap::Square), Some(Wrap::TopBottom), Some(Wrap::Behind), Some(Wrap::InFront)];
            let aligns = [HAlign::Start, HAlign::Center, HAlign::End];
            let items: Vec<MenuItem> = INSERT.iter().enumerate().map(|(i, s)| match i {
                20..=24 => {
                    let m = MenuItem::new(*s).enabled(pic.is_some()).radio(pic.is_some_and(|f| f.map(|f| f.wrap) == wraps[i - 20]));
                    if i == 20 { m.separated() } else { m }
                }
                25..=27 => {
                    let m = MenuItem::new(*s).enabled(pic.is_some_and(|f| f.is_some())).radio(pic.flatten().is_some_and(|f| f.h_rel == Rel::Text && f.h_align == aligns[i - 25]));
                    if i == 25 { m.separated() } else { m }
                }
                28..=33 => {
                    let m = MenuItem::new(*s).radio(PAGE_CMDS[i - 28].is(&page));
                    if i == 28 || i == 30 || i == 32 { m.separated() } else { m }
                }
                12 => MenuItem::new(if has_toc { "Update Table of Contents" } else { *s }).separated(),
                14 => MenuItem::new(*s).enabled(!in_table).separated(),
                15 => MenuItem::new(*s).enabled(!in_table),
                16 => MenuItem::new(*s).radio(sec.columns == 1).separated(),
                17 | 18 => MenuItem::new(*s).radio(sec.columns as usize == i - 15),
                19 => MenuItem::new(*s).checked(sec.rule).enabled(sec.columns > 1),
                _ => MenuItem::new(*s).enabled(match i {
                    2..=5 => in_table,
                    9 => on_note,
                    _ => true,
                }),
            }).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Insert".to_string(), self.insert_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::insert(i))).at(x, 8.0).width(96.0).render(ctx);
            x += 100.0;
        }
        {
            let tracking = self.doc.borrow().ed.borrow().doc.tracking;
            let items: Vec<MenuItem> = TOOLS.iter().enumerate().map(|(i, s)| match i {
                2 => MenuItem::new(*s).checked(tracking).separated(),
                4 => MenuItem::new(*s).separated(),
                _ => MenuItem::new(*s),
            }).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Tools".to_string(), self.tools_menu.clone(), items, move |i| cmds.lock().unwrap().push([Cmd::FindOpen, Cmd::SpellOpen, Cmd::TrackToggle, Cmd::ReviewOpen, Cmd::CompareWith, Cmd::MergeWith, Cmd::PasswordOpen][i.min(6)])).at(x, 8.0).width(88.0).render(ctx);
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

        // The cross-reference bar: a heading, and what of it to show.
        let xb = self.xref.get_cloned();
        if xb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let targets = ed.borrow().ref_targets();
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 120.0, 20.0), content: "Refer to".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let mut x = 96.0;
            Button::secondary("‹").at(x, y + 8.0).width(32.0).on_click(push(Cmd::XrefMove(false))).render(ctx);
            x += 36.0;
            let nw = (w - x - 36.0 - 380.0).max(120.0);
            let name = targets.get(xb.at).map_or(String::new(), |(_, lvl, text)| format!("{}{}", "  ".repeat(lvl.saturating_sub(1) as usize), text));
            ctx.add(Node::Rect { rect: Rect::new(x, y + 8.0, nw, 32.0), fill: t.bg_canvas(), radius: 6.0 });
            ctx.add(Node::Text { rect: Rect::new(x + 10.0, y + 15.0, nw - 20.0, 18.0), content: fit_text(&name, nw - 20.0, &st(type_size::MD, Weight::Regular, t.text_primary())), style: st(type_size::MD, Weight::Regular, t.text_primary()) });
            x += nw + 4.0;
            Button::secondary("›").at(x, y + 8.0).width(32.0).on_click(push(Cmd::XrefMove(true))).render(ctx);
            x += 44.0;
            ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, 60.0, 18.0), content: format!("{} of {}", xb.at + 1, targets.len()), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            x += 64.0;
            Button::primary("Page Number").at(x, y + 8.0).width(112.0).on_click(push(Cmd::XrefInsert(true))).render(ctx);
            x += 116.0;
            Button::secondary("Heading Text").at(x, y + 8.0).width(112.0).on_click(push(Cmd::XrefInsert(false))).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::XrefClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The note bar: the note's text on one line, paragraphs marked ¶.
        let nb = self.note.get_cloned();
        if nb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 120.0, 20.0), content: nb.title.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 12.0 - 128.0 - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.note_text.clone()).placeholder("Note text").autofocus(true).at(140.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::NoteDone)).render(ctx);
            Button::primary("Done").at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::NoteDone)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::NoteClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The comment bar: the first comment's text (or a reply) on one line.
        let cb = self.comment.get_cloned();
        if cb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let title = if cb.replying { "Reply".to_string() } else { cb.title.clone() };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 200.0, 20.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 220.0 - 388.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.comment_text.clone()).placeholder(if cb.replying { "Reply" } else { "Comment" }).autofocus(true).at(220.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::CommentDone)).render(ctx);
            let bx = w - 380.0;
            Button::primary("Done").at(bx, y + 8.0).width(64.0).on_click(push(Cmd::CommentDone)).render(ctx);
            Button::secondary("Reply").at(bx + 68.0, y + 8.0).width(64.0).on_click(push(Cmd::CommentReply)).render(ctx);
            Button::secondary(if cb.done { "Reopen" } else { "Resolve" }).at(bx + 136.0, y + 8.0).width(80.0).on_click(push(Cmd::CommentResolve)).render(ctx);
            Button::secondary("Delete").at(bx + 220.0, y + 8.0).width(72.0).on_click(push(Cmd::CommentDelete)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::CommentClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The review bar: the picked edit in a sentence, and what to do with it.
        let (reviewing, edits, picked, author, tracking) = {
            let mut e = ed.borrow_mut();
            let edits = e.edits();
            let picked = e.review_edit();
            let author = picked.as_ref().and_then(|x| e.doc.revisions.get(x.rev)).map(|r| r.author.to_string()).unwrap_or_default();
            (e.review, edits, picked, author, e.doc.tracking)
        };
        if reviewing {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let line = match &picked {
                Some(p) if matches!(p.kind, EditKind::Conflict(_)) => format!("Conflict · edit {} of {} · {}", edits.iter().position(|x| x == p).map_or(0, |i| i + 1), edits.len(), p.summary),
                Some(p) => format!("Edit {} of {} · {author} · {}", edits.iter().position(|x| x == p).map_or(0, |i| i + 1), edits.len(), p.summary),
                None => "No changes to review".to_string(),
            };
            let conflict = picked.as_ref().is_some_and(|p| matches!(p.kind, EditKind::Conflict(_)));
            // A conflict's three choices take more room than accept and reject.
            let extra = if conflict { 200.0 } else { 0.0 };
            let bx = w - 540.0 - extra;
            // As much of the line as fits before the buttons.
            let style = st(type_size::MD, Weight::Semibold, t.text_primary());
            let room = (bx - 24.0).max(0.0);
            let line = fit_text(&line, room, &style);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, room, 20.0), content: line, style });
            Button::secondary("‹").at(bx, y + 8.0).width(32.0).on_click(push(Cmd::ReviewPrevious)).render(ctx);
            Button::secondary("›").at(bx + 36.0, y + 8.0).width(32.0).on_click(push(Cmd::ReviewNext)).render(ctx);
            if conflict {
                // A conflict: whose version stays.
                let (mine, theirs) = picked.as_ref().map(|p| p.summary.split(" · ").map(|x| x.split(':').next().unwrap_or("").to_string()).collect::<Vec<_>>()).map(|v| (v.first().cloned().unwrap_or_default(), v.get(1).cloned().unwrap_or_default())).unwrap_or_default();
                let short = |s: &str| if s.chars().count() > 10 { s.chars().take(9).chain(std::iter::once('…')).collect() } else { s.to_string() };
                let bx2 = bx;
                Button::primary(&format!("Keep {}", short(&mine))).at(bx2 + 76.0, y + 8.0).width(118.0).on_click(push(Cmd::ReviewTake(Take::Mine))).render(ctx);
                Button::secondary(&format!("Take {}", short(&theirs))).at(bx2 + 198.0, y + 8.0).width(118.0).on_click(push(Cmd::ReviewTake(Take::Theirs))).render(ctx);
                Button::secondary("Keep Both").at(bx2 + 320.0, y + 8.0).width(92.0).on_click(push(Cmd::ReviewTake(Take::Both))).render(ctx);
            } else {
                Button::primary("Accept").at(bx + 76.0, y + 8.0).width(72.0).on_click(push(Cmd::ReviewAccept)).render(ctx);
                Button::secondary("Reject").at(bx + 152.0, y + 8.0).width(68.0).on_click(push(Cmd::ReviewReject)).render(ctx);
            }
            Button::secondary("Accept All").at(bx + extra + 228.0, y + 8.0).width(96.0).on_click(push(Cmd::ReviewAcceptAll)).render(ctx);
            Button::secondary("Reject All").at(bx + extra + 328.0, y + 8.0).width(92.0).on_click(push(Cmd::ReviewRejectAll)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::ReviewClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }
        // The password bar, over any other bar.
        let pb = self.password.get_cloned();
        if pb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let lw = 300.0f32.min(w * 0.4);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, lw, 20.0), content: pb.prompt.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 24.0 - lw - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.password_text.clone()).placeholder("Password").autofocus(true).at(lw + 20.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::PasswordDone)).render(ctx);
            Button::primary(if pb.setting { "Set" } else { "Open" }).at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordDone)).render(ctx);
            Button::secondary("Cancel").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }


        // The pages.
        let cmds = self.cmds.clone();
        let cmds2 = self.cmds.clone();
        RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).on_find(move || cmds.lock().unwrap().push(Cmd::FindOpen)).on_comment(move |id| cmds2.lock().unwrap().push(Cmd::CommentOpen(id))).emit(ctx);

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
            let mut pos = format!("Page {page} of {total} · {words}");
            // The page reads clean; the count says what is tracked.
            if !edits.is_empty() {
                let mut people: Vec<officina_text::model::RevId> = Vec::new();
                let mut names: Vec<&str> = Vec::new();
                for x in edits.iter() {
                    if !people.contains(&x.rev) {
                        people.push(x.rev);
                        if let Some(r) = e.doc.revisions.get(x.rev) {
                            if !names.contains(&&*r.author) {
                                names.push(&r.author);
                            }
                        }
                    }
                }
                let n = edits.len();
                let things = if n == 1 { "1 edit".to_string() } else { format!("{n} edits") };
                pos += &if names.len() > 1 { format!(" · {things} by {} people", names.len()) } else { format!(" · {things}") };
            }
            if tracking {
                pos += " · Tracking changes";
            }
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
    fn contents_and_cross_reference_commands() {
        let mut d = officina_text::model::TextDoc::new();
        d.blocks.remove(0);
        let h1 = d.styles.by_name("Heading 1").unwrap();
        d.push_para("Intro", 0, vec![]);
        d.push_para("Alpha", h1, vec![]);
        d.push_para("Body.", 0, vec![]);
        d.push_para("Beta", h1, vec![]);
        let doc = Doc::from_text_doc(d, "t.owrite");
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let ed = w.doc.borrow().ed.clone();
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(1, 0), false);
        run(Cmd::insert(12));
        assert_eq!(officina_text::fields::toc_range(&ed.borrow().doc, None), Some((1, 3)));
        run(Cmd::insert(12));
        assert_eq!(w.doc.borrow().status, "The table of contents is up to date");
        // Refer to Beta's page from the end of Body.
        let n = ed.borrow().doc.para(4).unwrap().text.len() as u32;
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(4, n), false);
        run(Cmd::insert(13));
        assert!(w.xref.get_cloned().open);
        assert!(w.pages_rect(1200.0, 800.0).y() > TOOLBAR_H);
        run(Cmd::XrefMove(true));
        run(Cmd::XrefInsert(false));
        let e = ed.borrow();
        let p = e.doc.para(4).unwrap();
        assert_eq!(e.doc.field_text(e.doc.fields_in(p)[0].1), "Beta");
        drop(e);
        run(Cmd::XrefClose);
        assert!(!w.xref.get_cloned().open);
    }

    #[test]
    fn password_bar_commands() {
        let doc = Doc::from_text("n.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        w.cmds.lock().unwrap().push(Cmd::PasswordOpen);
        assert!(w.take_cmds().is_empty());
        let pb = w.password.get_cloned();
        assert!(pb.open && pb.setting && pb.prompt.starts_with("Password to protect"));
        assert!(w.password_text.get_cloned().is_secret());
        // Done goes to the event loop, which takes the text.
        w.password_text.set(TextEdit::new("hunter2").secret(true));
        w.cmds.lock().unwrap().push(Cmd::PasswordDone);
        assert_eq!(w.take_cmds(), vec![Cmd::PasswordDone]);
        let (bar, text) = w.take_password();
        assert!(bar.setting && text == "hunter2");
        assert!(!w.password.get_cloned().open && w.password_text.get_cloned().text().is_empty());
        // Cancel closes the bar and tells the event loop.
        w.ask_password("“a.docx” is protected. Its password", false);
        w.cmds.lock().unwrap().push(Cmd::PasswordClose);
        assert_eq!(w.take_cmds(), vec![Cmd::PasswordClose]);
        assert!(!w.password.get_cloned().open);
    }

    #[test]
    fn note_bar_commands() {
        let doc = Doc::from_text("n.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 9), false);
        run(Cmd::insert(7));
        let nb = w.note.get_cloned();
        assert!(nb.open && nb.title == "Footnote 1");
        w.note_text.set(TextEdit::new("First paragraph ¶ second one"));
        run(Cmd::NoteDone);
        assert!(!w.note.get_cloned().open);
        assert_eq!(w.doc.borrow().ed.borrow().note_text(nb.id).unwrap(), "First paragraph\nsecond one");
        run(Cmd::insert(9));
        assert_eq!(w.note_text.get_cloned().text(), "First paragraph ¶ second one");
        run(Cmd::NoteClose);
        run(Cmd::insert(8));
        assert_eq!(w.note.get_cloned().title, "Endnote i");
        run(Cmd::Undo);
        run(Cmd::Undo);
        assert_eq!(w.doc.borrow().ed.borrow().note_text(nb.id).unwrap(), "");
    }

    #[test]
    fn page_commands_change_the_carets_section() {
        let doc = Doc::from_text("p.txt", b"One.\nTwo.\nThree.\nFour.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let page = || w.doc.borrow().ed.borrow().section_page();
        let base = page();
        assert!(base.width < base.height);
        // One section: the document's page turns.
        run(Cmd::insert(29));
        assert!(page().width > page().height);
        assert_eq!(w.doc.borrow().ed.borrow().doc.page, page());
        run(Cmd::Undo);
        assert_eq!(page(), base);
        // Two sections: only the caret's.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(2, 0), false);
        run(Cmd::insert(14));
        run(Cmd::insert(29));
        run(Cmd::insert(33));
        let land = page();
        assert!(land.width > land.height && land.margin_start == 720, "{land:?}");
        assert_eq!(w.doc.borrow().ed.borrow().doc.page, base, "the document's page stays");
        {
            let d = w.doc.borrow();
            let mut e = d.ed.borrow_mut();
            while !e.paginate(std::time::Duration::from_secs(5)) {}
            let sizes: Vec<i32> = e.pages.pages().iter().map(|p| p.setup.width).collect();
            assert_eq!(sizes, vec![base.width, land.width]);
        }
        // Back to portrait and normal margins: the section's page goes.
        run(Cmd::insert(28));
        run(Cmd::insert(32));
        assert_eq!(page(), base);
        assert!(w.doc.borrow().ed.borrow().doc.para(2).unwrap().props.section.unwrap().page.is_none());
        assert!(PAGE_CMDS[2].is(&base) || PAGE_CMDS[3].is(&base));
    }

    #[test]
    fn review_bar_commands() {
        let doc = Doc::from_text("r.txt", b"The quick brown fox.\nIt jumps over the dog.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        run(Cmd::TrackToggle);
        assert!(w.doc.borrow().ed.borrow().doc.tracking);
        {
            let d = w.doc.borrow();
            let mut e = d.ed.borrow_mut();
            e.author = "Ann".into();
            e.set_caret(pergola_richtext::Pos::new(0, 4), false);
            e.set_caret(pergola_richtext::Pos::new(0, 9), true);
            e.insert("fast");
            e.set_caret(pergola_richtext::Pos::new(1, 18), false);
            e.set_caret(pergola_richtext::Pos::new(1, 21), true);
            e.insert("cat");
        }
        assert_eq!(text(), vec!["The fast brown fox.", "It jumps over the cat."]);
        let top = w.pages_rect(1200.0, 800.0).y();
        // Review starts at the edit at or after the caret.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        run(Cmd::ReviewOpen);
        assert!(w.pages_rect(1200.0, 800.0).y() > top, "the review bar takes the bars' place");
        assert_eq!(w.doc.borrow().ed.borrow().review_at, 0);
        assert_eq!(w.doc.borrow().ed.borrow_mut().edits().len(), 2);
        // Reject the first, accept the second: the document is clean.
        run(Cmd::ReviewReject);
        assert_eq!(text()[0], "The quick brown fox.");
        assert_eq!(w.doc.borrow().ed.borrow_mut().edits().len(), 1);
        run(Cmd::ReviewAccept);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the cat."]);
        assert!(w.doc.borrow().ed.borrow_mut().edits().is_empty());
        assert!(w.doc.borrow().ed.borrow().doc.live_deletions().is_empty());
        // Undo brings the edit back; Reject All takes everything out.
        run(Cmd::Undo);
        run(Cmd::ReviewRejectAll);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the dog."]);
        // Opening another bar closes review.
        run(Cmd::FindOpen);
        assert!(!w.doc.borrow().ed.borrow().review);
        run(Cmd::TrackToggle);
        assert!(!w.doc.borrow().ed.borrow().doc.tracking);
    }

    #[test]
    fn compare_with_an_earlier_version() {
        let doc = Doc::from_text("new.txt", b"The slow brown fox.\nIt leaps over the dog.\nA new line.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        let older = Doc::from_text("old.txt", b"The quick brown fox.\nIt jumps over the dog.").unwrap();
        w.compare_with(&older, "old.txt");
        assert_eq!(w.doc.borrow().status, "Compared with “old.txt”: 3 edits.");
        assert!(w.doc.borrow().ed.borrow().review);
        assert_eq!(text(), vec!["The slow brown fox.", "It leaps over the dog.", "A new line."]);
        run(Cmd::ReviewRejectAll);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the dog."]);
        run(Cmd::Undo);
        run(Cmd::Undo);
        assert!(w.doc.borrow().ed.borrow_mut().edits().is_empty());
        // The Tools menu asks for the file.
        w.cmds.lock().unwrap().push(Cmd::CompareWith);
        assert_eq!(w.take_cmds(), vec![Cmd::CompareWith]);
    }

    #[test]
    fn merge_a_reviewed_copy_and_settle_its_conflict() {
        let doc = Doc::from_text("ann.txt", b"Pay within thirty days.\nDeliver in two weeks.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        // Ann's copy: her edit tracked against the original.
        w.compare_with(&Doc::from_text("original.txt", b"Pay within sixty days.\nDeliver in two weeks.").unwrap(), "original.txt");
        let ben = Doc::from_text("ben.txt", b"Pay within ninety days.\nDeliver in five weeks.").unwrap();
        w.merge_with(&ben, "ben.txt");
        assert_eq!(w.doc.borrow().status, "Merged “ben.txt”: 1 change added, 1 conflict to settle.");
        assert_eq!(text(), vec!["Pay within thirty days.", "Deliver in five weeks."]);
        run(Cmd::ReviewTake(Take::Theirs));
        assert_eq!(text(), vec!["Pay within ninety days.", "Deliver in five weeks."]);
        run(Cmd::Undo);
        run(Cmd::ReviewTake(Take::Both));
        assert_eq!(text(), vec!["Pay within thirty ninety days.", "Deliver in five weeks."]);
        // Not a copy of the same original.
        let x = Doc::from_text("x.txt", b"Something else.").unwrap();
        x.ed.borrow_mut().compare_with(&Doc::from_text("y.txt", b"Something old.").unwrap().ed.borrow().doc);
        w.merge_with(&x, "x.txt");
        assert!(w.doc.borrow().status.contains("isn't a copy"), "{}", w.doc.borrow().status);
        w.cmds.lock().unwrap().push(Cmd::MergeWith);
        assert_eq!(w.take_cmds(), vec![Cmd::MergeWith]);
    }

    #[test]
    fn comment_bar_commands() {
        let doc = Doc::from_text("c.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let comments = || w.doc.borrow().ed.borrow().doc.live_comments();
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 6), false);
        run(Cmd::insert(10));
        let cb = w.comment.get_cloned();
        assert!(cb.open && !cb.replying);
        // Left empty, the new comment goes away.
        run(Cmd::CommentDone);
        assert!(comments().is_empty());
        run(Cmd::insert(10));
        w.comment_text.set(TextEdit::new("Check this ¶ twice"));
        run(Cmd::CommentReply);
        let cb = w.comment.get_cloned();
        assert!(cb.replying);
        w.comment_text.set(TextEdit::new("Checked."));
        run(Cmd::CommentDone);
        let ids = comments();
        assert_eq!(ids.len(), 2);
        {
            let d = w.doc.borrow();
            let e = d.ed.borrow();
            assert_eq!(e.doc.comments.get(ids[0]).unwrap().text, "Check this\ntwice");
            assert_eq!(e.doc.comments.get(ids[1]).unwrap().parent, Some(ids[0]));
            assert_eq!(e.doc.comment_ranges(), vec![(ids[0], (0, 5), (0, 9))]);
        }
        // Edit Comment opens on the thread at the caret; a balloon click on a reply opens its thread.
        run(Cmd::CommentClose);
        run(Cmd::insert(11));
        assert_eq!(w.comment.get_cloned().id, ids[0]);
        assert!(w.comment.get_cloned().title.ends_with("1 reply"));
        run(Cmd::CommentClose);
        run(Cmd::CommentOpen(ids[1]));
        assert_eq!(w.comment.get_cloned().id, ids[0]);
        run(Cmd::CommentResolve);
        assert!(w.comment.get_cloned().done);
        assert!(w.doc.borrow().ed.borrow().doc.comments.get(ids[0]).unwrap().done);
        run(Cmd::CommentDelete);
        assert!(comments().is_empty());
        assert_eq!(w.doc.borrow().ed.borrow().doc.para(0).unwrap().text, "Some text here.");
        run(Cmd::Undo);
        assert_eq!(comments().len(), 2);
    }

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
