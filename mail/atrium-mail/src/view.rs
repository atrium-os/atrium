//! Drawing the window with Pergola (mail.md §15.2): places on the left
//! (224 pt), the list in the middle (400 pt), the reader in the rest.
//! Placed by hand in points that grow with the text size, as Scrinium's
//! window is; the list's rows and the reader's lines are virtual lists, so
//! only what is in view is in the scene.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mail_engine::Engine;
use mail_store::{flag, FolderId, MsgId, Role, Row};
use pergola::geom::{Axis, Rect};
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{ScrollState, VirtualList};
use pergola::{Color, Event, Key, KeyEventKind, Mutable, TextStyle};

pub fn side_w() -> f32 { em(224.0) }
pub fn list_w() -> f32 { em(400.0) }
/// A list row: sender and time, subject, preview.
pub fn row_h() -> f32 { em(72.0) }
pub fn place_h() -> f32 { em(30.0) }
pub fn head_h() -> f32 { em(48.0) }
/// About 80 characters (§15.4): the reader's text is no wider.
pub fn read_max() -> f32 { em(600.0) }
fn line_h() -> f32 { em(type_size::MD) * 1.45 }

/// One place in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub folder: FolderId,
    pub label: String,
    /// Shown above an account's first folder when there is more than one.
    pub account: Option<String>,
    pub count: usize,
}

/// What the reader shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub msg: MsgId,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
    /// The plain text, or a note in its place.
    pub text: String,
    /// The text isn't here yet, or there is only HTML (M3 shows that).
    pub note: Option<String>,
}

/// The window's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ui {
    pub places: Vec<Place>,
    pub folder: Option<FolderId>,
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
    pages: HashMap<usize, Vec<Row>>,
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

impl Mail {
    /// The row at `i` of the chosen folder, reading its page if needed.
    pub fn row(&self, i: usize) -> Option<Row> {
        let ui = self.ui.get_cloned();
        let folder = ui.folder?;
        let mut p = self.pages.borrow_mut();
        if p.gen != ui.gen || p.folder != Some(folder) {
            *p = Pages { gen: ui.gen, folder: Some(folder), pages: HashMap::new() };
        }
        let page = i / PAGE;
        if !p.pages.contains_key(&page) {
            let rows = self.engine.store.list(folder, page * PAGE, PAGE).unwrap_or_default();
            p.pages.insert(page, rows);
        }
        p.pages.get(&page).and_then(|r| r.get(i % PAGE)).cloned()
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
        ui.count = ui.folder.and_then(|f| store.count(f).ok()).unwrap_or(0);
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
        ui.count = self.engine.store.count(folder).unwrap_or(0);
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

    /// Select row `i` and show its message.
    pub fn open(&self, i: usize) {
        let Some(row) = self.row(i) else { return };
        let mut ui = self.ui.get_cloned();
        ui.cursor = Some(i);
        ui.reading = Some(self.shown(&row));
        self.ui.set(ui);
        self.reader.set(ScrollState::default());
    }

    fn shown(&self, row: &Row) -> Shown {
        let addrs = |l: &[mail_schema::Address]| l.iter().map(crate::who).collect::<Vec<_>>().join(", ");
        match self.engine.open(row.msg) {
            Ok(r) => {
                let s = &r.summary;
                let note = match (&r.text, r.has_html) {
                    (None, _) => Some("Only the headers are here so far; the text comes with the next sync.".to_string()),
                    (Some(t), true) if t.trim().is_empty() => Some("This message is written in HTML, which Mail will show soon. It has no plain-text version.".to_string()),
                    _ => None,
                };
                Shown {
                    msg: row.msg,
                    subject: if s.subject.trim().is_empty() { "(no subject)".into() } else { s.subject.clone() },
                    from: addrs(&s.from),
                    to: addrs(&s.to),
                    date: s.date.map(crate::full_date).unwrap_or_default(),
                    text: r.text.unwrap_or_default(),
                    note,
                }
            }
            Err(e) => Shown {
                msg: row.msg,
                subject: row.subject.clone(),
                from: row.from.clone(),
                to: String::new(),
                date: crate::full_date(row.date),
                text: String::new(),
                note: Some(format!("Couldn't open this message: {e}")),
            },
        }
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

pub struct MailView {
    pub mail: Mail,
    /// The reader's text broken into lines: (message, width, lines).
    wrapped: Rc<RefCell<Option<(MsgId, u32, Rc<Vec<String>>)>>>,
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
        MailView { mail, wrapped: Rc::default() }
    }

}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose middle is at `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) });
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
            text(ctx, 20.0, y + place_h() / 2.0, &fit(&p.label, type_size::SM, if sel { Weight::Semibold } else { Weight::Regular }, w - 48.0 - nw), type_size::SM, if sel { Weight::Semibold } else { Weight::Regular }, t.text_primary());
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
        text(ctx, x0 + 16.0, head_h() / 2.0, &fit(&title, type_size::LG, Weight::Semibold, w - 32.0), type_size::LG, Weight::Semibold, t.text_primary());
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
            let row = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w - 1.0, row_h()), 0.0));
            ctx.set_flex(row, FlexStyle { free: true, ..FlexStyle::default() });
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
            let unread = r.flags & flag::SEEN == 0;
            let wt = if unread { Weight::Semibold } else { Weight::Regular };
            let date = crate::date_text(r.date, now);
            let dw = width(&date, type_size::XS, Weight::Regular);
            let inner = w - 1.0 - 36.0;
            let line = em(20.0);
            let top = (row_h() - 3.0 * line) / 2.0;
            text(ctx, 20.0, top + line * 0.5, &fit(&r.from, type_size::SM, wt, inner - dw - 12.0), type_size::SM, wt, t.text_primary());
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
        // Headers: subject, sender, recipients and date.
        let mut y = em(20.0);
        text(ctx, x, y + em(14.0), &fit(&s.subject, type_size::XL, Weight::Semibold, tw), type_size::XL, Weight::Semibold, t.text_primary());
        y += em(40.0);
        let date_w = width(&s.date, type_size::XS, Weight::Regular);
        text(ctx, x, y + em(10.0), &fit(&s.from, type_size::SM, Weight::Semibold, tw - date_w - 12.0), type_size::SM, Weight::Semibold, t.text_primary());
        text(ctx, x + tw - date_w, y + em(10.0), &s.date, type_size::XS, Weight::Regular, t.text_secondary());
        y += em(22.0);
        if !s.to.is_empty() {
            text(ctx, x, y + em(10.0), &fit(&format!("To {}", s.to), type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, t.text_secondary());
            y += em(22.0);
        }
        y += em(8.0);
        rect(ctx, Rect::new(x, y, tw, 1.0), t.border_default());
        y += em(12.0);
        if let Some(n) = &s.note {
            let lines = pergola::text::lines(n, &sans(type_size::SM, Weight::Regular, t.text_secondary()), tw);
            for l in lines {
                text(ctx, x, y + em(10.0), &l, type_size::SM, Weight::Regular, t.text_secondary());
                y += em(22.0);
            }
            y += em(8.0);
        }
        if s.text.is_empty() {
            return;
        }
        // The text, broken to the column once per width.
        let key = (s.msg, tw.to_bits());
        let lines = {
            let mut c = self.wrapped.borrow_mut();
            match &*c {
                Some((m, wb, l)) if (*m, *wb) == key => l.clone(),
                _ => {
                    let style = sans(type_size::MD, Weight::Regular, t.text_primary());
                    let l = Rc::new(pergola::text::lines(&s.text.replace("\r\n", "\n"), &style, tw));
                    *c = Some((key.0, key.1, l.clone()));
                    l
                }
            }
        };
        let (n, lh) = (lines.len(), line_h());
        VirtualList::new(self.mail.reader.clone(), Rect::new(x0, y, w, h - y), n, lh, move |i, ctx| {
            let t = ctx.theme;
            let row = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w, lh), 0.0));
            ctx.set_flex(row, FlexStyle { free: true, ..FlexStyle::default() });
            text(ctx, x - x0, lh / 2.0, &lines[i], type_size::MD, Weight::Regular, t.text_primary());
            ctx.pop();
        })
        .key(200)
        .backdrop(t.bg_canvas())
        .emit(ctx);
    }
}
