//! Drawing the window with Pergola (mail.md §15.2): places on the left
//! (224 pt), the list in the middle (400 pt), the reader in the rest.
//! Placed by hand in points that grow with the text size, as Scrinium's
//! window is; the list's rows and the reader's lines are virtual lists, so
//! only what is in view is in the scene.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mail_engine::Engine;
use mail_store::{flag, FolderId, MsgId, Role, ThreadId};
use pergola::geom::{Axis, Rect};
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{ScrollState, VirtualList};
use pergola::{Color, Event, Key, KeyEventKind, Mutable, TextStyle};

use crate::plain::{self, Plain};
use crate::sender::{self, Check, Sender};

pub fn side_w() -> f32 { em(224.0) }
pub fn list_w() -> f32 { em(400.0) }
/// A list row: sender and time, subject, preview.
pub fn row_h() -> f32 { em(72.0) }
pub fn place_h() -> f32 { em(30.0) }
pub fn head_h() -> f32 { em(48.0) }
/// About 80 characters (§15.4): the reader's text is no wider.
pub fn read_max() -> f32 { em(600.0) }
/// One line of the reader: every item in it is this tall.
pub fn line_h() -> f32 { em(type_size::MD) * 1.5 }
/// The reader's heading: subject and people.
pub fn read_head_h() -> f32 { em(84.0) }
/// Indent per quote level, with its bar.
fn quote_w() -> f32 { em(14.0) }

/// One place in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub folder: FolderId,
    pub label: String,
    /// Shown above an account's first folder when there is more than one.
    pub account: Option<String>,
    pub count: usize,
}

/// One row of the list: a conversation or a message.
#[derive(Debug, Clone, PartialEq)]
pub struct ListRow {
    pub thread: Option<ThreadId>,
    pub msg: MsgId,
    pub people: String,
    /// Messages in the conversation (1 for a message).
    pub count: usize,
    pub date: i64,
    pub subject: String,
    pub preview: String,
    pub unread: bool,
    pub flagged: bool,
}

/// A message's body in the reader.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Body {
    pub plain: Option<Plain>,
    /// Said in place of the text, or before it.
    pub note: Option<String>,
    pub show_quoted: bool,
    pub show_signature: bool,
}

/// A message in the reader.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub msg: MsgId,
    pub sender: Sender,
    pub to: String,
    pub date: String,
    pub preview: String,
    /// Open (its body shown) or folded to one line.
    pub open: bool,
    /// Unread when the conversation was opened: the "new" line goes before
    /// the first of them.
    pub new: bool,
    pub body: Option<Body>,
}

/// What the reader shows: a conversation, or one message.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub subject: String,
    pub people: String,
    pub msgs: Vec<Msg>,
    /// Bumped on every change, for the laid-out lines.
    pub version: u64,
}

impl Shown {
    /// The message most recently opened (the one keys act on).
    pub fn current(&self) -> Option<&Msg> {
        self.msgs.iter().rev().find(|m| m.open)
    }
}

/// The window's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ui {
    pub places: Vec<Place>,
    pub folder: Option<FolderId>,
    /// Conversations (true) or messages in the chosen folder.
    pub conversations: bool,
    /// Rows in the chosen folder.
    pub count: usize,
    /// The selected row.
    pub cursor: Option<usize>,
    pub reading: Option<Shown>,
    /// Bumped whenever the index may have changed, to drop cached rows.
    pub gen: u64,
    pub error: Option<String>,
}

const PAGE: usize = 100;

/// Rows read from the index a page at a time, for one `gen`.
#[derive(Default)]
struct Pages {
    gen: u64,
    folder: Option<FolderId>,
    pages: HashMap<usize, Vec<ListRow>>,
}

/// The engine and the state, shared by the view and its handlers.
#[derive(Clone)]
pub struct Mail {
    pub engine: Rc<Engine>,
    pub ui: Mutable<Ui>,
    pages: Rc<RefCell<Pages>>,
    /// The list's scroll, to keep the cursor in view on j/k.
    list: Mutable<ScrollState>,
    reader: Mutable<ScrollState>,
    size: Mutable<(f32, f32)>,
    now: Rc<dyn Fn() -> i64>,
}

fn addrs(l: &[mail_schema::Address]) -> String {
    l.iter().map(sender::name_of).collect::<Vec<_>>().join(", ")
}

impl Mail {
    fn read_page(&self, folder: FolderId, conversations: bool, page: usize) -> Vec<ListRow> {
        let store = &self.engine.store;
        if conversations {
            store.conversations(folder, page * PAGE, PAGE).unwrap_or_default().into_iter().map(|c| ListRow {
                thread: Some(c.thread),
                msg: c.newest,
                people: c.people.join(", "),
                count: c.count,
                date: c.date,
                subject: c.subject,
                preview: c.preview,
                unread: c.unread,
                flagged: c.flagged,
            }).collect()
        } else {
            store.list(folder, page * PAGE, PAGE).unwrap_or_default().into_iter().map(|r| ListRow {
                thread: None,
                msg: r.msg,
                people: r.from,
                count: 1,
                date: r.date,
                subject: r.subject,
                preview: r.preview,
                unread: r.flags & flag::SEEN == 0,
                flagged: r.flags & flag::FLAGGED != 0,
            }).collect()
        }
    }

    /// The row at `i` of the chosen folder, reading its page if needed.
    pub fn row(&self, i: usize) -> Option<ListRow> {
        let ui = self.ui.get_cloned();
        let folder = ui.folder?;
        let mut p = self.pages.borrow_mut();
        if p.gen != ui.gen || p.folder != Some(folder) {
            *p = Pages { gen: ui.gen, folder: Some(folder), pages: HashMap::new() };
        }
        let page = i / PAGE;
        if !p.pages.contains_key(&page) {
            let rows = self.read_page(folder, ui.conversations, page);
            p.pages.insert(page, rows);
        }
        p.pages.get(&page).and_then(|r| r.get(i % PAGE)).cloned()
    }

    fn count_of(&self, folder: FolderId, conversations: bool) -> usize {
        let s = &self.engine.store;
        if conversations { s.conversation_count(folder) } else { s.count(folder) }.unwrap_or(0)
    }

    /// Read the places again from the index (after a sync), keeping the
    /// chosen folder; the first time, choose the first inbox.
    pub fn refresh(&self) {
        let mut ui = self.ui.get_cloned();
        let store = &self.engine.store;
        let accounts = match store.accounts() {
            Ok(a) => a,
            Err(e) => {
                ui.error = Some(format!("Couldn't read the mail index: {e}"));
                self.ui.set(ui);
                return;
            }
        };
        let many = accounts.len() > 1;
        let mut places = Vec::new();
        for (aid, a) in &accounts {
            let mut first = true;
            for (fid, f) in store.folders(*aid).unwrap_or_default() {
                if f.hidden {
                    continue;
                }
                places.push(Place {
                    folder: fid,
                    label: crate::folder_label(&f.name, f.role),
                    account: (many && first).then(|| a.name.clone()),
                    count: store.count(fid).unwrap_or(0),
                });
                first = false;
            }
        }
        if ui.folder.is_none() || !places.iter().any(|p| Some(p.folder) == ui.folder) {
            let inbox = accounts.iter().find_map(|(aid, _)| {
                store.folders(*aid).ok()?.into_iter().find(|(_, f)| f.role == Role::Inbox && !f.hidden).map(|(id, _)| id)
            });
            ui.folder = inbox.or(places.first().map(|p| p.folder));
            ui.cursor = None;
            ui.reading = None;
        }
        if let Some(f) = ui.folder {
            ui.conversations = store.view(f).unwrap_or(true);
            ui.count = self.count_of(f, ui.conversations);
        } else {
            ui.count = 0;
        }
        if ui.cursor.is_some_and(|c| c >= ui.count) {
            ui.cursor = None;
            ui.reading = None;
        }
        ui.places = places;
        ui.gen += 1;
        ui.error = None;
        self.ui.set(ui);
    }

    pub fn choose_folder(&self, folder: FolderId) {
        let mut ui = self.ui.get_cloned();
        if ui.folder == Some(folder) {
            return;
        }
        ui.folder = Some(folder);
        ui.conversations = self.engine.store.view(folder).unwrap_or(true);
        ui.count = self.count_of(folder, ui.conversations);
        ui.cursor = None;
        ui.reading = None;
        ui.gen += 1;
        self.ui.set(ui);
        self.list.set(ScrollState::default());
    }

    /// Conversations or messages for the chosen folder, remembered (§11.7).
    pub fn set_conversations(&self, on: bool) {
        let mut ui = self.ui.get_cloned();
        let Some(f) = ui.folder else { return };
        if ui.conversations == on {
            return;
        }
        if let Err(e) = self.engine.store.write(|w| w.set_view(f, on)) {
            ui.error = Some(format!("Couldn't keep that choice: {e}"));
        }
        ui.conversations = on;
        ui.count = self.count_of(f, on);
        ui.cursor = None;
        ui.reading = None;
        ui.gen += 1;
        self.ui.set(ui);
        self.list.set(ScrollState::default());
    }

    /// The window is now `w` x `h` points.
    pub fn set_size(&self, w: f32, h: f32) {
        if self.size.get() != (w, h) {
            self.size.set((w, h));
        }
    }

    /// A message's body, parsed alone in its own jail.
    fn body(&self, msg: MsgId) -> Body {
        match self.engine.open(msg) {
            Ok(r) => {
                let note = match (&r.text, r.has_html) {
                    (None, _) => Some("Only the headers are here so far; the text comes with the next sync.".to_string()),
                    (Some(t), true) if t.trim().is_empty() => Some("This message is written in HTML, which Mail will show soon. It has no plain-text version.".to_string()),
                    _ => None,
                };
                let plain = r.text.filter(|t| !t.trim().is_empty()).map(|t| plain::parse(&t));
                Body { plain, note, show_quoted: false, show_signature: false }
            }
            Err(e) => Body { plain: None, note: Some(format!("Couldn't open this message: {e}")), ..Body::default() },
        }
    }

    fn msg_of(&self, msg: MsgId, m: &mail_store::Message, flags: u32) -> Msg {
        let s = &m.summary;
        Msg {
            msg,
            sender: sender::sender(s),
            to: addrs(&s.to),
            date: crate::full_date(m.date),
            preview: s.preview.clone(),
            open: false,
            new: flags & flag::SEEN == 0,
            body: None,
        }
    }

    /// Select row `i` and show its conversation or message: the newest
    /// message and any unread ones open, the others folded (§15.2).
    pub fn open(&self, i: usize) {
        let Some(row) = self.row(i) else { return };
        let store = &self.engine.store;
        let mut msgs: Vec<Msg> = match row.thread {
            Some(t) => store.thread(t).unwrap_or_default().iter().map(|m| self.msg_of(m.msg, &m.message, m.flags)).collect(),
            None => Vec::new(),
        };
        if msgs.is_empty() {
            if let Ok(m) = store.message(row.msg) {
                msgs.push(self.msg_of(row.msg, &m, if row.unread { 0 } else { flag::SEEN }));
            }
        }
        let n = msgs.len();
        for (k, m) in msgs.iter_mut().enumerate() {
            if m.new || k + 1 == n {
                m.open = true;
                m.body = Some(self.body(m.msg));
            }
        }
        let mut people: Vec<String> = Vec::new();
        for m in &msgs {
            if !people.contains(&m.sender.name) {
                people.push(m.sender.name.clone());
            }
        }
        let subject = msgs.first().and_then(|m| store.message(m.msg).ok()).map(|m| m.summary.subject).unwrap_or(row.subject);
        let mut ui = self.ui.get_cloned();
        let version = ui.reading.as_ref().map_or(0, |r| r.version) + 1;
        ui.cursor = Some(i);
        ui.reading = Some(Shown {
            subject: if subject.trim().is_empty() { "(no subject)".into() } else { subject },
            people: if n > 1 { format!("{} · {n} messages", people.join(", ")) } else { String::new() },
            msgs,
            version,
        });
        self.ui.set(ui);
        self.reader.set(ScrollState::default());
    }

    fn change_reading(&self, f: impl FnOnce(&mut Shown)) {
        let mut ui = self.ui.get_cloned();
        if let Some(r) = &mut ui.reading {
            f(r);
            r.version += 1;
            self.ui.set(ui);
        }
    }

    /// Open a folded message, or fold an open one.
    pub fn toggle(&self, k: usize) {
        let body = {
            let ui = self.ui.get_cloned();
            match ui.reading.as_ref().and_then(|r| r.msgs.get(k)) {
                Some(m) if !m.open && m.body.is_none() => Some(self.body(m.msg)),
                _ => None,
            }
        };
        self.change_reading(|r| {
            if let Some(m) = r.msgs.get_mut(k) {
                m.open = !m.open;
                if body.is_some() {
                    m.body = body;
                }
            }
        });
    }

    /// Show or hide a message's quoted tail or signature.
    pub fn unfold(&self, k: usize, quoted: bool) {
        self.change_reading(|r| {
            if let Some(b) = r.msgs.get_mut(k).and_then(|m| m.body.as_mut()) {
                if quoted { b.show_quoted = !b.show_quoted } else { b.show_signature = !b.show_signature }
            }
        });
    }

    /// j/k and the arrows move through the list and open as they go.
    /// Whether the key was Mail's.
    pub fn key(&self, ev: &Event) -> bool {
        let Event::Key { kind, key, chars, modifiers } = ev else { return false };
        if *kind == KeyEventKind::Up || modifiers.ctrl || modifiers.alt || modifiers.meta {
            return false;
        }
        let ui = self.ui.get_cloned();
        if ui.count == 0 {
            return false;
        }
        let next = match (key, chars.as_str()) {
            (Key::Down, _) | (Key::Char, "j") => ui.cursor.map_or(0, |c| (c + 1).min(ui.count - 1)),
            (Key::Up, _) | (Key::Char, "k") => ui.cursor.map_or(0, |c| c.saturating_sub(1)),
            (Key::Home, _) => 0,
            (Key::End, _) => ui.count - 1,
            _ => return false,
        };
        if ui.cursor != Some(next) {
            self.open(next);
            let shown = self.size.get().1 - head_h();
            VirtualList::reveal(&self.list, next, ui.count, row_h(), shown);
        }
        true
    }
}

/// One line of the reader.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A folded message: sender, preview, date.
    Folded(usize),
    /// An open message's sender line.
    From(usize),
    To(usize),
    RepliesTo(usize),
    /// A note (headers only, HTML only, couldn't open), one line of it.
    Note(String),
    /// "New since you last looked" (§11.1).
    New,
    /// One line of text: its quote depth and its spans (text, link).
    Text { depth: u8, spans: Vec<(String, Option<String>)> },
    /// "Show quoted text" / "Show signature" (`shown`: "Hide …").
    Fold { msg: usize, quoted: bool, shown: bool },
    Gap,
}

/// The reader's lines for a column `w` wide.
pub fn lay_out(r: &Shown, w: f32) -> Vec<Item> {
    let mut out = Vec::new();
    let first_new = r.msgs.iter().position(|m| m.new).filter(|&k| k > 0);
    for (k, m) in r.msgs.iter().enumerate() {
        if Some(k) == first_new {
            out.push(Item::New);
        }
        if !m.open {
            out.push(Item::Folded(k));
            continue;
        }
        if k > 0 {
            out.push(Item::Gap);
        }
        out.push(Item::From(k));
        if !m.to.is_empty() {
            out.push(Item::To(k));
        }
        if m.sender.replies_to.is_some() {
            out.push(Item::RepliesTo(k));
        }
        out.push(Item::Gap);
        let Some(b) = &m.body else { continue };
        if let Some(n) = &b.note {
            let st = sans(type_size::SM, Weight::Regular, Color::TRANSPARENT);
            for l in pergola::text::lines(n, &st, w) {
                out.push(Item::Note(l));
            }
        }
        if let Some(p) = &b.plain {
            let st = sans(type_size::MD, Weight::Regular, Color::TRANSPARENT);
            let sig = p.signature_from.unwrap_or(p.lines.len());
            let quoted = p.quoted_from.unwrap_or(sig).min(sig);
            for (i, line) in p.lines.iter().enumerate() {
                if i == quoted && quoted < sig {
                    out.push(Item::Fold { msg: k, quoted: true, shown: b.show_quoted });
                    if !b.show_quoted {
                        continue;
                    }
                }
                if i == sig {
                    out.push(Item::Fold { msg: k, quoted: false, shown: b.show_signature });
                }
                if (i >= quoted && i < sig && !b.show_quoted) || (i >= sig && !b.show_signature) {
                    continue;
                }
                wrap_line(line, w - line.depth as f32 * quote_w(), &st, &mut out);
            }
        }
    }
    out.push(Item::Gap);
    out
}

/// Break one line to `w`, keeping which parts are links.
fn wrap_line(line: &plain::Line, w: f32, st: &TextStyle, out: &mut Vec<Item>) {
    if line.text.trim().is_empty() {
        out.push(Item::Text { depth: line.depth, spans: Vec::new() });
        return;
    }
    let pieces = pergola::text::lines(&line.text, st, w.max(em(80.0)));
    let mut at = 0;
    for piece in pieces {
        let Some(off) = line.text[at..].find(piece.as_str()) else {
            out.push(Item::Text { depth: line.depth, spans: vec![(piece, None)] });
            continue;
        };
        let (s, e) = (at + off, at + off + piece.len());
        at = e;
        let mut spans = Vec::new();
        let mut cur = s;
        for (ls, le, url) in &line.links {
            let (a, b) = ((*ls).max(s), (*le).min(e));
            if a >= b {
                continue;
            }
            if a > cur {
                spans.push((line.text[cur..a].to_string(), None));
            }
            spans.push((line.text[a..b].to_string(), Some(url.clone())));
            cur = b;
        }
        if cur < e {
            spans.push((line.text[cur..e].to_string(), None));
        }
        out.push(Item::Text { depth: line.depth, spans });
    }
}

pub struct MailView {
    pub mail: Mail,
    /// The reader laid out: (version, width, lines).
    laid: Rc<RefCell<Option<(u64, u32, Rc<Vec<Item>>)>>>,
}

impl MailView {
    /// The view, with every cell on the app's `dirty` flag.
    pub fn new(engine: Rc<Engine>, dirty: std::sync::Arc<std::sync::atomic::AtomicBool>, size: (f32, f32), now: impl Fn() -> i64 + 'static) -> MailView {
        let ui = Mutable::with_dirty(Ui::default(), dirty);
        let mail = Mail {
            engine,
            list: ui.sibling(ScrollState::default()),
            reader: ui.sibling(ScrollState::default()),
            size: ui.sibling(size),
            ui,
            pages: Rc::default(),
            now: Rc::new(now),
        };
        mail.refresh();
        MailView { mail, laid: Rc::default() }
    }
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose middle is at `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> pergola::NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

/// `s` cut with "…" to fit `max`.
fn fit(s: &str, size: f32, weight: Weight, max: f32) -> String {
    let s = s.replace(['\r', '\n', '\t'], " ");
    if max <= 0.0 {
        return String::new();
    }
    if width(&s, size, weight) <= max {
        return s;
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t, size, weight) <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

fn rect(ctx: &mut Ctx, r: Rect, fill: Color) {
    ctx.add(Node::Rect { rect: r, fill, radius: 0.0 });
}

/// A pressable area with its hover; children follow until `ctx.pop()`.
fn area(ctx: &mut Ctx, r: Rect, fill: Color, on: impl Fn() + 'static) {
    let id = ctx.push(Node::Rect { rect: r, fill, radius: 5.0 });
    if ctx.is_hovered(id) || ctx.is_pressed(id) {
        let c = if ctx.is_pressed(id) { ctx.theme.state_pressed() } else if fill == Color::TRANSPARENT { ctx.theme.state_hover() } else { fill };
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
            *fill = c;
        }
    }
    ctx.on_click(id, on);
}

/// A row of a virtual list: a free stack `w` x `h`; children follow until `ctx.pop()`.
fn row(ctx: &mut Ctx, w: f32, h: f32) {
    let r = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w, h), 0.0));
    ctx.set_flex(r, FlexStyle { free: true, ..FlexStyle::default() });
}

impl View for MailView {
    fn render(&self, ctx: &mut Ctx) {
        let ui = self.mail.ui.get_cloned();
        let (w, h) = self.mail.size.get();
        let t = ctx.theme;
        ctx.min_size(pergola::geom::Size::new(em(160.0) + em(280.0) + em(320.0), head_h() + 4.0 * row_h()));
        rect(ctx, Rect::new(0.0, 0.0, w, h), t.bg_canvas());
        // The places and the list keep their widths while the reader has
        // room; in a narrow window they give it up first.
        let side = side_w().min(w * 0.22).max(em(160.0).min(w * 0.3));
        let list = list_w().min(w * 0.36).max(em(280.0).min(w * 0.4));
        self.places(ctx, &ui, side, h);
        self.list(ctx, &ui, side, list, h);
        self.reader(ctx, &ui, side + list, w - side - list, h);
    }
}

impl MailView {
    fn places(&self, ctx: &mut Ctx, ui: &Ui, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(0.0, 0.0, w, h), t.bg_window());
        rect(ctx, Rect::new(w - 1.0, 0.0, 1.0, h), t.border_default());
        text(ctx, 16.0, head_h() / 2.0, "Mail", type_size::LG, Weight::Semibold, t.text_primary());
        let mut y = head_h();
        for p in &ui.places {
            if let Some(a) = &p.account {
                y += em(8.0);
                text(ctx, 16.0, y + em(10.0), &fit(&a.to_uppercase(), type_size::XS, Weight::Semibold, w - 32.0), type_size::XS, Weight::Semibold, t.text_tertiary());
                y += em(20.0);
            }
            let sel = ui.folder == Some(p.folder);
            let (m, f) = (self.mail.clone(), p.folder);
            area(ctx, Rect::new(8.0, y + 1.0, w - 16.0, place_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.choose_folder(f));
            let n = if p.count > 0 { p.count.to_string() } else { String::new() };
            let nw = width(&n, type_size::SM, Weight::Regular);
            let wt = if sel { Weight::Semibold } else { Weight::Regular };
            text(ctx, 20.0, y + place_h() / 2.0, &fit(&p.label, type_size::SM, wt, w - 48.0 - nw), type_size::SM, wt, t.text_primary());
            text(ctx, w - 20.0 - nw, y + place_h() / 2.0, &n, type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
            y += place_h();
            if y > h {
                break;
            }
        }
        if ui.places.is_empty() {
            text(ctx, 16.0, y + em(16.0), "No accounts yet", type_size::SM, Weight::Regular, t.text_secondary());
        }
    }

    fn list(&self, ctx: &mut Ctx, ui: &Ui, x0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(x0 + w - 1.0, 0.0, 1.0, h), t.border_default());
        let title = ui.places.iter().find(|p| Some(p.folder) == ui.folder).map(|p| p.label.clone()).unwrap_or_default();
        // The Conversations | Messages switch at the header's right (§11.7).
        let mut sw_x = x0 + w - 16.0;
        if ui.folder.is_some() {
            for (label, on) in [("Messages", false), ("Conversations", true)] {
                let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
                sw_x -= lw;
                let sel = ui.conversations == on;
                let m = self.mail.clone();
                area(ctx, Rect::new(sw_x, head_h() / 2.0 - em(12.0), lw, em(24.0)), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.set_conversations(on));
                text(ctx, sw_x + em(8.0), head_h() / 2.0, label, type_size::XS, Weight::Medium, if sel { t.text_primary() } else { t.text_secondary() });
                ctx.pop();
                sw_x -= em(2.0);
            }
        }
        text(ctx, x0 + 16.0, head_h() / 2.0, &fit(&title, type_size::LG, Weight::Semibold, sw_x - x0 - 24.0), type_size::LG, Weight::Semibold, t.text_primary());
        if let Some(e) = &ui.error {
            text(ctx, x0 + 16.0, head_h() + em(20.0), &fit(e, type_size::SM, Weight::Regular, w - 32.0), type_size::SM, Weight::Regular, t.danger_fg());
            return;
        }
        if ui.count == 0 {
            if ui.folder.is_some() {
                text(ctx, x0 + 16.0, head_h() + em(20.0), "Nothing here", type_size::SM, Weight::Regular, t.text_secondary());
            }
            return;
        }
        let m = self.mail.clone();
        let cursor = ui.cursor;
        let now = (self.mail.now)();
        VirtualList::new(self.mail.list.clone(), Rect::new(x0, head_h(), w - 1.0, h - head_h()), ui.count, row_h(), move |i, ctx| {
            let t = ctx.theme;
            row(ctx, w - 1.0, row_h());
            let Some(r) = m.row(i) else {
                ctx.pop();
                return;
            };
            let sel = cursor == Some(i);
            let mm = m.clone();
            area(ctx, Rect::new(6.0, 1.0, w - 13.0, row_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || mm.open(i));
            if sel {
                ctx.add(Node::Rect { rect: Rect::new(6.0, 1.0, 3.0, row_h() - 2.0), fill: t.accent_fg(), radius: 1.5 });
            }
            let wt = if r.unread { Weight::Semibold } else { Weight::Regular };
            let date = crate::date_text(r.date, now);
            let dw = width(&date, type_size::XS, Weight::Regular);
            let count = if r.count > 1 { format!(" {}", r.count) } else { String::new() };
            let cw = width(&count, type_size::XS, Weight::Regular);
            let inner = w - 1.0 - 36.0;
            let line = em(20.0);
            let top = (row_h() - 3.0 * line) / 2.0;
            let people = fit(&r.people, type_size::SM, wt, inner - dw - cw - 12.0);
            let pw = width(&people, type_size::SM, wt);
            text(ctx, 20.0, top + line * 0.5, &people, type_size::SM, wt, t.text_primary());
            if !count.is_empty() {
                text(ctx, 20.0 + pw, top + line * 0.5, &count, type_size::XS, Weight::Regular, t.text_secondary());
            }
            text(ctx, w - 17.0 - dw, top + line * 0.5, &date, type_size::XS, Weight::Regular, t.text_secondary());
            let subject = if r.subject.trim().is_empty() { "(no subject)" } else { &r.subject };
            text(ctx, 20.0, top + line * 1.5, &fit(subject, type_size::SM, wt, inner), type_size::SM, wt, t.text_primary());
            text(ctx, 20.0, top + line * 2.5, &fit(&r.preview, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
            ctx.pop();
        })
        .key(100)
        .backdrop(t.bg_canvas())
        .emit(ctx);
    }

    fn reader(&self, ctx: &mut Ctx, ui: &Ui, x0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        let Some(s) = &ui.reading else {
            if ui.count > 0 {
                let msg = "Choose a message to read";
                text(ctx, x0 + (w - width(msg, type_size::SM, Weight::Regular)) / 2.0, h / 2.0, msg, type_size::SM, Weight::Regular, t.text_tertiary());
            }
            return;
        };
        let col = w.min(read_max() + 64.0);
        let x = x0 + (w - col) / 2.0 + 32.0;
        let tw = col - 64.0;
        // The heading: the subject in full, who is in it.
        text(ctx, x, em(34.0), &fit(&s.subject, type_size::XL, Weight::Semibold, tw), type_size::XL, Weight::Semibold, t.text_primary());
        if !s.people.is_empty() {
            text(ctx, x, em(62.0), &fit(&s.people, type_size::SM, Weight::Regular, tw), type_size::SM, Weight::Regular, t.text_secondary());
        }
        rect(ctx, Rect::new(x, read_head_h() - 1.0, tw, 1.0), t.border_default());
        let key = (s.version, tw.to_bits());
        let items = {
            let mut c = self.laid.borrow_mut();
            match &*c {
                Some((v, wb, l)) if (*v, *wb) == key => l.clone(),
                _ => {
                    let l = Rc::new(lay_out(s, tw));
                    *c = Some((key.0, key.1, l.clone()));
                    l
                }
            }
        };
        let shown = Rc::new(s.clone());
        let (m, lh, xi) = (self.mail.clone(), line_h(), x - x0);
        VirtualList::new(self.mail.reader.clone(), Rect::new(x0, read_head_h(), w, h - read_head_h()), items.len(), lh, move |i, ctx| {
            row(ctx, w, lh);
            draw_item(ctx, &items[i], &shown, &m, xi, tw, lh);
            ctx.pop();
        })
        .key(200)
        .backdrop(t.bg_canvas())
        .emit(ctx);
    }
}

fn draw_item(ctx: &mut Ctx, item: &Item, s: &Shown, mail: &Mail, x: f32, tw: f32, lh: f32) {
    let t = ctx.theme;
    let cy = lh / 2.0;
    match item {
        Item::Folded(k) => {
            let msg = &s.msgs[*k];
            let mm = mail.clone();
            let k = *k;
            area(ctx, Rect::new(x - 8.0, 1.0, tw + 16.0, lh - 2.0), Color::TRANSPARENT, move || mm.toggle(k));
            let dw = width(&msg.date, type_size::XS, Weight::Regular);
            let wt = if msg.new { Weight::Semibold } else { Weight::Regular };
            let name = fit(&msg.sender.name, type_size::SM, wt, em(160.0));
            let nw = width(&name, type_size::SM, wt);
            text(ctx, x, cy, &name, type_size::SM, wt, t.text_primary());
            text(ctx, x + nw + em(10.0), cy, &fit(&msg.preview, type_size::SM, Weight::Regular, tw - nw - dw - em(24.0)), type_size::SM, Weight::Regular, t.text_secondary());
            text(ctx, x + tw - dw, cy, &msg.date, type_size::XS, Weight::Regular, t.text_secondary());
            ctx.pop();
        }
        Item::From(k) => {
            let msg = &s.msgs[*k];
            let dw = width(&msg.date, type_size::XS, Weight::Regular);
            // A press on an open message's sender line folds it (not the last one).
            if *k + 1 < s.msgs.len() {
                let (mm, k) = (mail.clone(), *k);
                area(ctx, Rect::new(x - 8.0, 1.0, tw + 16.0, lh - 2.0), Color::TRANSPARENT, move || mm.toggle(k));
            }
            let name = fit(&msg.sender.name, type_size::SM, Weight::Semibold, tw * 0.6);
            let nw = width(&name, type_size::SM, Weight::Semibold);
            text(ctx, x, cy, &name, type_size::SM, Weight::Semibold, t.text_primary());
            let (mark, color) = match msg.sender.check {
                Check::Verified => (format!("{} ✓", msg.sender.detail), t.success_fg()),
                Check::NotVerified => (if msg.sender.detail.is_empty() { "not verified".to_string() } else { format!("{} · not verified", msg.sender.detail) }, t.warning_fg()),
                Check::NotYet => (msg.sender.detail.clone(), t.text_secondary()),
            };
            let mark = if mark.trim().is_empty() { String::new() } else { format!("· {mark}") };
            text(ctx, x + nw + em(6.0), cy, &fit(&mark, type_size::SM, Weight::Regular, tw - nw - dw - em(20.0)), type_size::SM, Weight::Regular, color);
            text(ctx, x + tw - dw, cy, &msg.date, type_size::XS, Weight::Regular, t.text_secondary());
            if *k + 1 < s.msgs.len() {
                ctx.pop();
            }
        }
        Item::To(k) => {
            text(ctx, x, cy, &fit(&format!("To {}", s.msgs[*k].to), type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, t.text_secondary());
        }
        Item::RepliesTo(k) => {
            let r = s.msgs[*k].sender.replies_to.clone().unwrap_or_default();
            text(ctx, x, cy, &fit(&format!("Replies go to {r}"), type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, t.warning_fg());
        }
        Item::Note(l) => {
            text(ctx, x, cy, l, type_size::SM, Weight::Regular, t.text_secondary());
        }
        Item::New => {
            let label = "New since you last looked";
            let lw = width(label, type_size::XS, Weight::Semibold);
            rect(ctx, Rect::new(x, cy, (tw - lw) / 2.0 - em(8.0), 1.0), t.accent_fg());
            text(ctx, x + (tw - lw) / 2.0, cy, label, type_size::XS, Weight::Semibold, t.accent_fg());
            rect(ctx, Rect::new(x + (tw + lw) / 2.0 + em(8.0), cy, (tw - lw) / 2.0 - em(8.0), 1.0), t.accent_fg());
        }
        Item::Text { depth, spans } => {
            for d in 0..*depth {
                rect(ctx, Rect::new(x + d as f32 * quote_w() + 2.0, 0.0, 2.0, lh), t.border_strong());
            }
            let mut tx = x + *depth as f32 * quote_w();
            let color = if *depth > 0 { t.text_secondary() } else { t.text_primary() };
            for (s, link) in spans {
                let id = text(ctx, tx, cy, s, type_size::MD, Weight::Regular, if link.is_some() { t.accent_fg() } else { color });
                if let Some(url) = link {
                    let url = url.clone();
                    ctx.on_click(id, move || eprintln!("atrium-mail: link {url}"));
                }
                tx += width(s, type_size::MD, Weight::Regular);
            }
        }
        Item::Fold { msg, quoted, shown } => {
            let label = match (quoted, shown) {
                (true, false) => "Show quoted text",
                (true, true) => "Hide quoted text",
                (false, false) => "Show signature",
                (false, true) => "Hide signature",
            };
            let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
            let (mm, k, q) = (mail.clone(), *msg, *quoted);
            area(ctx, Rect::new(x, cy - em(11.0), lw, em(22.0)), t.bg_window(), move || mm.unfold(k, q));
            text(ctx, x + em(8.0), cy, label, type_size::XS, Weight::Medium, t.text_secondary());
            ctx.pop();
        }
        Item::Gap => {}
    }
}
