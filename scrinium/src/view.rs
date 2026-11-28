//! Drawing Scrinium (scrinium.md §3.3, §4.2) with Pergola: the file
//! manager's window, and the picker in an app's slot. Both are one
//! [`Browser`]; the picker shows fewer controls and a Choose button.
//!
//! Sizes follow the visual language (list rows 30 px, toolbar 48, Phosphor
//! icons 16 and 20, one accent). Everything is placed by hand, as the
//! Navigator's window is, except the rows, which the list's scroll view
//! stacks (each a free stack drawn at its own origin).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use limen::picker::Kind;
use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{CredentialField, CredentialState, Glyph, ScrollState, TextField, ToolItem, Toolbar, VirtualList};
use pergola::{Color, Event, Key, KeyEventKind, Mutable, NodeId, TextStyle};

use crate::browser::{Browser, Edit, Mode, Mods, OpenSheet, Outcome};
use crate::connect::Step;
use crate::fs;

/// Phosphor codepoints (fonts/phosphor/Phosphor.ttf, regular weight).
pub mod ph {
    pub use pergola::widgets::phosphor::*;
    pub const ARROW_LEFT: char = '\u{e058}';
    pub const ARROW_UP: char = '\u{e08e}';
    pub const CARET_RIGHT: char = '\u{e13a}';
    pub const FILE_TEXT: char = '\u{e23a}';
    pub const FILM_STRIP: char = '\u{e792}';
    pub const WARNING: char = '\u{e4e0}';
    pub const LINK: char = '\u{e2e2}';
    pub const GLOBE: char = '\u{e288}';
    pub const FILE: char = '\u{e230}';
    pub const TABLE: char = '\u{e476}';
    pub const PRESENTATION: char = '\u{e654}';
}

// Layout sizes at 100% text; each grows with the system text size
// (resolution-independence.md §6.2), so rows and bars fit their labels.
fn em(pt: f32) -> f32 { pergola::theme::size::em(pt) }
pub fn side_w() -> f32 { em(208.0) }

/// The places' width in a window `w` wide: [`side_w`], but no more than
/// about a third of a narrow window, so the list keeps room for names
/// (a big text size on a small screen).
pub fn side_for(w: f32) -> f32 {
    side_w().min((w * 0.3).max(em(140.0)))
}

/// Whether a window `h` tall is too short for the actions on a row of
/// their own under the path: then they share the path's row, and what
/// doesn't fit goes into More (resolution-independence.md §6.3).
pub fn compact(h: f32) -> bool {
    h < toolbar_h() + actions_h() + header_h() + 4.0 * row_h() + status_h()
}
pub fn pick_side_w() -> f32 { em(176.0) }
/// The picker's places in a sheet `w` wide: [`pick_side_w`], but no more
/// than three tenths of it, so names keep their room in a sheet held
/// smaller than [`pick_min_size`] (by its monitor).
pub fn pick_side_for(w: f32) -> f32 {
    pick_side_w().min(w * 0.3)
}
/// The picker's natural minimum, in em so it grows with the text size:
/// its places, a name and a size column, its heading, path row, six rows
/// and its buttons. At text size 100 it is inside the catalogue's 640 x
/// 440, which frescod gives the sheet unless this is larger.
pub fn pick_min_size() -> pergola::geom::Size {
    pergola::geom::Size::new(pick_side_w() + em(420.0), pick_title_h() + pick_path_h() + 6.0 * row_h() + pick_foot_h())
}
pub fn toolbar_h() -> f32 { em(48.0) }
pub fn actions_h() -> f32 { em(40.0) }
pub fn strip_h() -> f32 { em(48.0) }
pub fn header_h() -> f32 { em(28.0) }
pub fn status_h() -> f32 { em(28.0) }
pub fn row_h() -> f32 { em(30.0) }
pub fn place_h() -> f32 { em(44.0) }

/// Quick look's viewer slot (an embed's place in the window).
pub const LOOK_SLOT: pergola::slot::SlotKey = 0x4c4f_4f4b;
/// Separate from the viewer, so the print sheet can cover the file window.
pub const PRINT_SLOT: pergola::slot::SlotKey = 0x5052_4e54;
/// Where a row's text starts: after its 16 px inset and its icon.
fn text_x() -> f32 { 16.0 + em(28.0) }
pub fn pick_title_h() -> f32 { em(52.0) }
pub fn pick_foot_h() -> f32 { em(60.0) }
/// The picker's path row, under its heading.
pub fn pick_path_h() -> f32 { em(40.0) }

pub struct ScriniumView {
    pub state: Mutable<Browser>,
    /// The name being typed (new folder, rename).
    pub field: Mutable<TextEdit>,
    pub scroll: Mutable<ScrollState>,
    crossing_scroll: Mutable<ScrollState>,
    pub size: Mutable<(f32, f32)>,
    /// The modifiers of the last press (the loop sets them from the raw
    /// event, which the click handler doesn't see).
    pub mods: Arc<Mutex<Mods>>,
    pub clock: Instant,
    /// Arca's password field on "Connect to server" (a slot: the value is
    /// typed into Arca's own surface, never this process).
    pub cred: CredentialField,
    /// What that field told the app: filled, and its handle.
    pub cred_state: Mutable<CredentialState>,
    /// Arca's recovery-code field on "Enter recovery code" (tessera-
    /// encryption.md §7.4): typed into Arca, redeemed by arcad.
    pub rcode: CredentialField,
    pub rcode_state: Mutable<CredentialState>,
    /// The actions row's "More" menu, where actions go that no longer fit.
    pub more: Mutable<pergola::widgets::MenuState>,
    /// The cursor row last kept in view: a move of the cursor (the
    /// arrows, Home, End) scrolls its row whole into the list.
    shown_cursor: Arc<Mutex<Option<usize>>>,
}

impl ScriniumView {
    pub fn new(state: Mutable<Browser>, field: Mutable<TextEdit>, scroll: Mutable<ScrollState>, size: Mutable<(f32, f32)>) -> Self {
        // On the app's dirty flag, as `state` is: typing in the password
        // field and moving in the More menu repaint.
        let cred_state = state.sibling(CredentialState::default());
        let cred = CredentialField::password(cred_state.clone());
        let rcode_state = state.sibling(CredentialState::default());
        let rcode = CredentialField::recovery_code(rcode_state.clone());
        let more = state.sibling(Default::default());
        Self { crossing_scroll: state.sibling(Default::default()), state, field, scroll, size, mods: Arc::new(Mutex::new(Mods::default())), clock: Instant::now(), cred, cred_state, rcode, rcode_state, more, shown_cursor: Arc::new(Mutex::new(None)) }
    }
}

/// A stick's second line in the sidebar: Locked, an older copy, whose
/// (another persona's chip; this window's own sealed sticks say
/// "Encrypted"), its free space.
pub fn place_sub(p: &crate::browser::Place, persona: &str) -> String {
    use crate::browser::persona_name;
    let sealed = !p.seal.persona.is_empty();
    let theirs = (sealed && p.seal.persona != persona).then(|| persona_name(&p.seal.persona));
    if p.seal.locked {
        return match (p.seal.older, theirs) {
            (true, _) => "Older copy · Locked".into(),
            (false, Some(w)) => format!("{w} · Locked"),
            (false, None) => "Locked".into(),
        };
    }
    if p.seal.older {
        return "Older copy".into();
    }
    let free = if p.size > 0 { format!("{} free", fs::size_text(p.free)) } else { "USB stick".into() };
    let free = match theirs {
        Some(w) => format!("{w} · {free}"),
        None if sealed => format!("Encrypted · {free}"),
        None => free,
    };
    if p.read_only { format!("{free} · read-only") } else { free }
}

/// Change the browser from a handler.
pub fn with(state: &Mutable<Browser>, f: impl FnOnce(&mut Browser)) {
    let mut b = state.get_cloned();
    f(&mut b);
    state.set(b);
}

/// Start an edit and fill the name field for it.
pub fn begin_edit(state: &Mutable<Browser>, field: &Mutable<TextEdit>, rename: bool) {
    with(state, |b| if rename { b.start_rename() } else { b.start_new_folder() });
    let text = match state.get_cloned().edit {
        Some(Edit::NewFolder) => "New folder".to_string(),
        Some(Edit::Rename(n)) => n,
        None => return,
    };
    let mut e = TextEdit::new(&text);
    e.select_all();
    field.set(e);
}

/// The keyboard map (scrinium.md §3.4). Returns whether the key was taken.
/// While a name is being typed, only Escape is the browser's.
pub fn on_key(state: &Mutable<Browser>, field: &Mutable<TextEdit>, ev: &Event) -> bool {
    let Event::Key { kind, key, modifiers: m, chars } = ev else { return false };
    if !kind.pressed() {
        return false;
    }
    let b = state.get_cloned();
    let picker = b.picker().is_some();
    if b.file_crossing.is_some() {
        if *key == Key::Escape { with(state, |b| b.file_crossing = None); }
        return true;
    }
    if b.connect.is_some() {
        if *key == Key::Escape {
            with(state, Browser::close_connect);
            return true;
        }
        return false;
    }
    if b.key_changed.is_some() {
        if *key == Key::Escape {
            with(state, Browser::close_key_changed);
        }
        return true;
    }
    if b.open_sheet.is_some() {
        if *key == Key::Escape {
            with(state, Browser::close_open_sheet);
        }
        return true;
    }
    if b.edit.is_some() || b.confirm_delete.is_some() {
        if *key == Key::Escape {
            with(state, |b| {
                b.edit = None;
                b.confirm_delete = None;
            });
            return true;
        }
        if b.confirm_delete.is_some() && *key == Key::Enter && *kind == KeyEventKind::Down {
            with(state, Browser::delete_confirmed);
            return true;
        }
        return false;
    }
    let c = chars.to_ascii_lowercase();
    match (key, m.ctrl, m.alt) {
        (Key::Up, false, false) => with(state, |b| b.step(-1, m.shift)),
        (Key::Down, false, false) => with(state, |b| b.step(1, m.shift)),
        (Key::Up, false, true) | (Key::Backspace, false, false) => with(state, Browser::up),
        (Key::Left, false, true) => with(state, Browser::go_back),
        (Key::Home, false, false) => with(state, |b| b.step(isize::MIN / 2, m.shift)),
        (Key::End, false, false) => with(state, |b| b.step(isize::MAX / 2, m.shift)),
        (Key::Enter, false, false) if *kind == KeyEventKind::Down => with(state, |b| match b.cursor.or(b.selected.first().copied()) {
            Some(i) if b.selected.len() <= 1 => b.activate(i),
            _ if b.picker().is_some() => b.choose(),
            _ => {}
        }),
        (Key::Escape, _, _) if picker => with(state, Browser::cancel),
        (Key::Escape, _, _) => with(state, |b| {
            b.selected.clear();
            b.notice = None;
        }),
        (Key::Delete, false, false) if !picker => with(state, Browser::ask_delete),
        (Key::Char, false, false) if chars == " " && !picker => with(state, Browser::toggle_look),
        (Key::Char, true, false) => match (c.as_str(), m.shift) {
            ("a", false) => with(state, Browser::select_all),
            ("c", false) if !picker => with(state, |b| b.copy(false)),
            ("x", false) if !picker => with(state, |b| b.copy(true)),
            ("v", false) if !picker => with(state, Browser::paste),
            ("n", true) if !picker => begin_edit(state, field, false),
            ("e", false) if !picker => begin_edit(state, field, true),
            ("h", false) => with(state, Browser::toggle_hidden),
            ("r", false) => with(state, Browser::reload),
            _ => return false,
        },
        _ => return false,
    }
    true
}

/// `on_key`, and whether the window's app must not see the key too: a
/// navigation key the browser took (an arrow moved the cursor, Space
/// opened quick look) would otherwise also scroll the list, which then
/// ends a line past the row the cursor's reveal showed. Other keys reach
/// the app as before (Tab and Enter for a sheet's buttons).
pub fn take_key(state: &Mutable<Browser>, field: &Mutable<TextEdit>, ev: &Event) -> bool {
    let nav = matches!(ev, Event::Key { key, chars, .. }
        if matches!(key, Key::Up | Key::Down | Key::Left | Key::Right | Key::PageUp | Key::PageDown | Key::Home | Key::End)
            || (*key == Key::Char && chars == " "));
    on_key(state, field, ev) && nav
}

// -- drawing helpers ---------------------------------------------------------

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose middle is at `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

/// The delete strip's question in `room` pt: the name gives up its length
/// first, then the second sentence goes. The limits are em, so at a large
/// text size the name keeps a readable stub (not two letters) before the
/// sentence is dropped.
pub fn delete_question(names: &[String], room: f32) -> String {
    if names.len() == 1 {
        let (with_tail, bare) = ("Delete “”? It can't be brought back.", "Delete “”?");
        let name_room = |shell: &str| (room - width(shell, type_size::SM, Weight::Medium)).min(em(220.0));
        if name_room(with_tail) >= em(60.0) {
            format!("Delete “{}”? It can't be brought back.", fit(&names[0], type_size::SM, Weight::Medium, name_room(with_tail)))
        } else {
            format!("Delete “{}”?", fit(&names[0], type_size::SM, Weight::Medium, name_room(bare).max(em(24.0))))
        }
    } else {
        let q = format!("Delete {} items? They can't be brought back.", names.len());
        if width(&q, type_size::SM, Weight::Medium) <= room { q } else { format!("Delete {} items?", names.len()) }
    }
}

/// `s`, cut with an ellipsis to fit `max` px.
/// `s` cut with an ellipsis to fit `max` pt in a monospace style.
fn fit_mono(s: &str, style: &TextStyle, max: f32) -> String {
    if Node::measure_text(s, style).w <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if Node::measure_text(&t, style).w <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

fn fit(s: &str, size: f32, weight: Weight, max: f32) -> String {
    if max <= 0.0 {
        return String::new();
    }
    if width(s, size, weight) <= max {
        return s.into();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t, size, weight) <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}

fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) -> NodeId {
    ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius })
}

fn icon(ctx: &mut Ctx, x: f32, y: f32, g: char, size: f32, color: Color) -> NodeId {
    Glyph::new(g).size(size).color(color).at(x, y).emit(ctx)
}

/// A pressable area, pushed as the parent of what is drawn in it (so a
/// press on its text or icon reaches it), `hover` while hovered. The
/// caller pops it.
fn area(ctx: &mut Ctx, r: Rect, radius: f32, fill: Color, hover: Color, on: impl Fn() + 'static) -> NodeId {
    let id = ctx.push(Node::Rect { rect: r, fill, radius });
    if ctx.is_hovered(id) || ctx.is_pressed(id) {
        let c = if ctx.is_pressed(id) { ctx.theme.state_pressed() } else { hover };
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
            *fill = c;
        }
    }
    ctx.on_click(id, on);
    id
}

/// A 28 pt ghost icon button (its glyph grows with the text size, so the
/// button does too).
fn icon_button(ctx: &mut Ctx, x: f32, y: f32, g: char, enabled: bool, tip: &str, on: impl Fn() + 'static) {
    let t = ctx.theme;
    let r = Rect::new(x, y, em(28.0), em(28.0));
    let id = if enabled { area(ctx, r, 6.0, Color::TRANSPARENT, t.state_hover(), on) } else { ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 6.0 }) };
    icon(ctx, x + em(4.0), y + em(4.0), g, 20.0, if enabled { t.text_secondary() } else { t.text_disabled() });
    ctx.pop();
    ctx.tooltip(id, tip.to_string());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Ghost,
    Secondary,
    Primary,
    Danger,
}

/// A text button 32 pt high (at 100% text) at (`x`, `y`); returns its width.
/// A [`button`]'s width.
fn button_w(label: &str) -> f32 {
    width(label, type_size::SM, Weight::Medium) + em(28.0)
}

fn button(ctx: &mut Ctx, x: f32, y: f32, label: &str, look: Look, enabled: bool, on: impl Fn() + 'static) -> f32 {
    let t = ctx.theme;
    let (w, h) = (button_w(label), em(32.0));
    let r = Rect::new(x, y, w, h);
    let (fill, hover, fg) = match look {
        Look::Ghost => (Color::TRANSPARENT, t.state_hover(), t.text_primary()),
        Look::Secondary => (t.bg_surface(), t.state_hover(), t.text_primary()),
        Look::Primary => (t.accent_fill(), t.accent_fill_hover(), t.text_on_accent()),
        Look::Danger => (t.danger_fg(), t.danger_fg(), t.text_on_accent()),
    };
    if look == Look::Secondary {
        rect(ctx, x, y, w, h, t.border_strong(), 6.0);
    }
    let inner = if look == Look::Secondary { Rect::new(x + 1.0, y + 1.0, w - 2.0, h - 2.0) } else { r };
    if enabled {
        area(ctx, inner, 6.0, fill, hover, on);
    } else {
        ctx.push(Node::Rect { rect: inner, fill: if look == Look::Primary || look == Look::Danger { t.state_hover() } else { fill }, radius: 6.0 });
    }
    text(ctx, x + em(14.0), y + h / 2.0, label, type_size::SM, Weight::Medium, if enabled { fg } else { t.text_disabled() });
    ctx.pop();
    w
}

fn file_icon(name: &str, dir: bool, link: bool) -> char {
    if link {
        return ph::LINK;
    }
    if dir {
        return ph::FOLDER;
    }
    use atrium_types::Family as F;
    match atrium_types::by_name(name) {
        Some(t) if t.mime == "application/pdf" => ph::FILE_PDF,
        Some(t) => match t.family {
            F::Image => ph::IMAGE,
            F::Audio => ph::MUSIC_NOTE_SIMPLE,
            F::Video => ph::FILM_STRIP,
            F::Archive | F::Bundle => ph::PACKAGE,
            F::Program => ph::TERMINAL_WINDOW,
            F::Link => ph::LINK,
            F::WebPage => ph::GLOBE,
            F::Spreadsheet => ph::TABLE,
            F::Presentation => ph::PRESENTATION,
            F::Text | F::Document => ph::FILE_TEXT,
        },
        None => ph::FILE,
    }
}

impl View for ScriniumView {
    fn render(&self, ctx: &mut Ctx) {
        let b = self.state.get_cloned();
        let (w, h) = self.size.get();
        let t = ctx.theme;
        rect(ctx, 0.0, 0.0, w, h, t.bg_canvas(), 0.0);
        if let Some(rows) = &b.recent_workbooks {
            use pergola::widgets::Button;
            let title = TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Semibold, color: t.text_primary() };
            let body = TextStyle { weight: Weight::Regular, ..title.clone() };
            ctx.add(Node::Text { rect: Rect::new(16.0, 14.0, w - 32.0, 24.0),
                content: "Recent Workbooks".into(), style: title });
            for (index, (id, name)) in rows.iter().enumerate() {
                let id = *id;
                let state = self.state.clone();
                Button::secondary(name.clone()).at(16.0, 48.0 + 35.0 * index as f32)
                    .width(w - 32.0).on_click(move || {
                        with(&state, |b| b.outcome = Some(Outcome::RecentWorkbook {
                            id, activation: pergola::clipboard::current_activation().unwrap_or_default(),
                        }));
                    }).render(ctx);
            }
            let message = b.notice.as_ref().map(|notice| notice.text.as_str())
                .or_else(|| rows.is_empty().then_some("No recent workbooks."));
            if let Some(message) = message {
                ctx.add(Node::Text { rect: Rect::new(16.0, h - 80.0, w - 32.0, 24.0),
                    content: message.into(), style: body });
            }
            let state = self.state.clone();
            Button::secondary("Close").at(w - 92.0, h - 46.0).width(76.0)
                .on_click(move || with(&state, |b| b.outcome = Some(Outcome::Cancelled))).render(ctx);
            return;
        }
        if let Some((name, versions)) = &b.versions {
            use pergola::widgets::Button;
            let title = TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Semibold, color: t.text_primary() };
            let body = TextStyle { weight: Weight::Regular, ..title.clone() };
            ctx.add(Node::Text { rect: Rect::new(16.0, 14.0, w - 32.0, 24.0),
                content: "Workbook Versions".into(), style: title });
            ctx.add(Node::Text { rect: Rect::new(16.0, 44.0, w - 32.0, 22.0),
                content: name.clone(), style: body.clone() });
            for (index, &generation) in versions.iter().enumerate() {
                let state = self.state.clone();
                Button::secondary(format!("Open generation {generation} read-only"))
                    .at(16.0, 76.0 + 36.0 * index as f32).on_click(move || {
                        with(&state, |b| b.outcome = Some(Outcome::WorkbookVersion {
                            generation, activation: pergola::clipboard::current_activation().unwrap_or_default(),
                        }));
                    }).render(ctx);
            }
            let reason = b.notice.as_ref().map(|notice| notice.text.as_str())
                .or_else(|| versions.is_empty().then_some("No retained prior saves are available."));
            if let Some(reason) = reason {
                ctx.add(Node::Text { rect: Rect::new(16.0, h - 80.0, w - 32.0, 24.0),
                    content: reason.into(), style: body });
            }
            let state = self.state.clone();
            Button::secondary("Close").at(w - 92.0, h - 46.0).width(76.0)
                .on_click(move || with(&state, |b| b.outcome = Some(Outcome::Cancelled))).render(ctx);
            return;
        }
        match &b.mode {
            Mode::Manager => {
                // Laid out by hand, so the minimum is stated: the places,
                // a name and a size column, a few rows (em: it grows with
                // the text size).
                // Short of that it shares the path's row with the actions
                // (`compact`); the minimum is what that layout needs.
                ctx.min_size(pergola::geom::Size::new(em(140.0) + em(360.0), toolbar_h() + header_h() + 3.0 * row_h() + status_h()));
                if let Some(ask) = &b.file_crossing {
                    ctx.min_size(pergola::geom::Size::new(em(500.0), em(300.0)));
                    self.crossing_sheet(ctx, ask, w, h);
                    return;
                }
                self.manager(ctx, &b, w, h);
                if b.connect.is_some() {
                    self.connect_sheet(ctx, &b, w, h);
                }
                if b.key_changed.is_some() {
                    self.key_changed_sheet(ctx, &b, w, h);
                }
                if b.open_sheet.is_some() {
                    self.open_sheet(ctx, &b, w, h);
                }
                if b.stick_sheet.is_some() {
                    self.stick_sheet(ctx, &b, w, h);
                }
            }
            Mode::Picker { .. } => {
                // The sheet grows to this when the text outgrows the
                // catalogue's size (resolution-independence.md §7.1).
                ctx.min_size(pick_min_size());
                self.picker(ctx, &b, w, h)
            }
        }
    }
}

impl ScriniumView {
    fn press_handler(&self, i: usize) -> impl Fn() + 'static {
        let (s, mods, clock) = (self.state.clone(), self.mods.clone(), self.clock);
        move || {
            let m = *mods.lock().unwrap();
            let now = clock.elapsed().as_secs_f64() * 1e3;
            with(&s, |b| b.press(i, m, now));
        }
    }

    /// The places down the side, from `top`, `sw` wide.
    fn sidebar(&self, ctx: &mut Ctx, b: &Browser, top: f32, sw: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, 0.0, top, sw, h - top, t.bg_surface(), 0.0);
        rect(ctx, sw - 1.0, top, 1.0, h - top, t.border_default(), 0.0);
        text(ctx, 16.0, top + em(22.0), "Places", type_size::XS, Weight::Semibold, t.text_tertiary());
        let manager = b.picker().is_none();
        if manager {
            self.servers(ctx, b, top, sw, h);
        }
        if b.places.is_empty() {
            text(ctx, 16.0, top + em(54.0), "Plug in a USB stick", type_size::SM, Weight::Regular, t.text_tertiary());
            return;
        }
        for (i, p) in b.places.iter().enumerate() {
            let y = top + em(38.0) + i as f32 * (place_h() + 2.0);
            let on = b.at == Some(i);
            let s = self.state.clone();
            ctx.key(1000 + i as u64);
            area(ctx, Rect::new(8.0, y, sw - 16.0, place_h()), 6.0, if on { t.selection_idle() } else { Color::TRANSPARENT }, t.state_hover(), move || with(&s, |b| b.open_place(i)));
            let g = if p.seal.locked { ph::LOCK_SIMPLE } else if p.network { ph::GLOBE } else if p.stick { ph::USB } else { ph::FOLDER };
            icon(ctx, 16.0, y + em(7.0), g, 20.0, if on { t.accent_fg() } else { t.text_secondary() });
            let room = sw - 16.0 - text_x() - if p.stick && manager { em(30.0) } else { 0.0 };
            text(ctx, text_x(), y + em(15.0), &fit(&p.label, type_size::SM, Weight::Medium, room), type_size::SM, Weight::Medium, t.text_primary());
            let sub = if p.network {
                let free = if p.offline {
                    "Reconnecting…".into()
                } else if p.size > 0 {
                    format!("Server · {} free", fs::size_text(p.free))
                } else {
                    "Server".into()
                };
                if p.read_only { format!("{free} · read-only") } else { free }
            } else if p.stick {
                place_sub(p, &b.persona)
            } else if let Some(who) = p.persona_folder() {
                // Both personas' folders are labelled (personas.md §4.1).
                crate::browser::persona_name(who)
            } else if p.id.starts_with("home:") && b.places.iter().any(|o| o.persona_folder().is_some()) {
                "Personal".into()
            } else {
                p.root.to_string_lossy().into_owned()
            };
            text(ctx, text_x(), y + em(31.0), &fit(&sub, type_size::XS, Weight::Regular, room), type_size::XS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            if p.stick && manager {
                let s = self.state.clone();
                icon_button(ctx, sw - 16.0 - em(28.0), y + em(8.0), ph::EJECT, true, if p.network { "Disconnect" } else { "Eject" }, move || with(&s, |b| b.eject(i)));
            }
        }
    }

    /// The foot of the sidebar: saved servers not connected now, and
    /// "Connect to server".
    fn servers(&self, ctx: &mut Ctx, b: &Browser, top: f32, sw: f32, h: f32) {
        let t = ctx.theme;
        let connected: Vec<&str> = b.places.iter().filter(|p| p.network).map(|p| p.label.as_str()).collect();
        let saved: Vec<(usize, String)> = b
            .saved
            .iter()
            .enumerate()
            .map(|(i, s)| (i, format!("{}@{}:{}", s.addr.user, s.addr.host, s.addr.path)))
            .filter(|(_, l)| !connected.contains(&l.as_str()))
            .collect();
        let foot = h - em(48.0);
        let mut y = foot - saved.len() as f32 * em(32.0);
        if !saved.is_empty() && y > top + 38.0 + b.places.len() as f32 * (place_h() + 2.0) + 24.0 {
            text(ctx, 16.0, y - 12.0, "Servers", type_size::XS, Weight::Semibold, t.text_tertiary());
            for (i, label) in saved {
                let s = self.state.clone();
                ctx.key(3000 + i as u64);
                area(ctx, Rect::new(8.0, y, sw - 16.0, em(30.0)), 6.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| b.reconnect(i)));
                icon(ctx, 16.0, y + em(7.0), ph::GLOBE, 16.0, t.text_tertiary());
                let tx = 24.0 + em(16.0);
                text(ctx, tx, y + em(15.0), &fit(&label, type_size::SM, Weight::Regular, sw - tx - 16.0), type_size::SM, Weight::Regular, t.text_secondary());
                ctx.pop();
                y += em(32.0);
            }
        }
        let s = self.state.clone();
        ctx.key(2999);
        area(ctx, Rect::new(8.0, foot + 8.0, sw - 16.0, em(32.0)), 6.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, Browser::open_connect));
        icon(ctx, 16.0, foot + 8.0 + em(6.0), ph::PLUS, 20.0, t.text_secondary());
        text(ctx, 24.0 + em(20.0), foot + 8.0 + em(16.0), "Connect to server", type_size::SM, Weight::Medium, t.text_primary());
        ctx.pop();
    }

    fn crossing_sheet(&self, ctx: &mut Ctx, ask: &crate::browser::FileCrossing, w: f32, h: f32) {
        let t = ctx.theme;
        let pad = em(22.0);
        let title = format!("{} between personas", ask.verb());
        text(ctx, pad, pad, &title, type_size::LG, Weight::Semibold, t.text_primary());
        let mut x = pad;
        for (i, owner) in [&ask.from, &ask.to].into_iter().enumerate() {
            if i == 1 {
                text(ctx, x, em(60.0), "→", type_size::SM, Weight::Medium, t.text_secondary());
                x += em(26.0);
            }
            let name = crate::browser::persona_name(owner);
            let wide = width(&name, type_size::SM, Weight::Medium) + em(20.0);
            rect(ctx, x, em(47.0), wide, em(26.0), t.bg_surface(), em(6.0));
            if let Some([r,g,b]) = ask.colours[i] {
                rect(ctx, x, em(47.0), em(4.0), em(26.0), Color::rgba(f32::from(r)/255.0, f32::from(g)/255.0, f32::from(b)/255.0, 1.0), em(2.0));
            }
            text(ctx, x + em(10.0), em(60.0), &name, type_size::SM, Weight::Medium, t.text_primary());
            x += wide + em(8.0);
        }
        let style = sans(type_size::SM, Weight::Regular, t.text_primary());
        let view_w = (w - 2.0 * pad).max(em(80.0));
        let mut top = em(88.0);
        if let Some(why) = &ask.refusal {
            for line in pergola::text::lines(why, &style, view_w) {
                text(ctx, pad, top, &line, type_size::SM, Weight::Medium, t.danger_fg());
                top += em(22.0);
            }
        }
        let lines: Vec<_> = ask.names().iter().flat_map(|name| pergola::text::lines(&format!("{name:?}"), &style, view_w - em(20.0))).collect();
        let rh = em(22.0);
        VirtualList::new(self.crossing_scroll.clone(), Rect::new(pad, top, view_w, (h - top - em(65.0)).max(rh)), lines.len(), rh, move |i, ctx| {
            text(ctx, 0.0, rh / 2.0, &lines[i], type_size::SM, Weight::Regular, ctx.theme.text_primary());
        }).key(4711).emit(ctx);
        let s = self.state.clone();
        let y = h - em(50.0);
        button(ctx, pad, y, if ask.refusal.is_some() { "Close" } else { "Cancel" }, Look::Ghost, true, move || with(&s, |b| b.file_crossing = None));
        if ask.refusal.is_none() {
            let s = self.state.clone();
            button(ctx, w - pad - button_w(ask.verb()), y, ask.verb(), Look::Primary, true, move || with(&s, Browser::confirm_file_crossing));
        }
    }

    /// A saved server shows another key: stop. There is no "connect anyway";
    /// the way on is to forget it and connect afresh, accepting the new key.
    fn key_changed_sheet(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        let Some(k) = &b.key_changed else { return };
        ctx.key(4100);
        area(ctx, Rect::new(0.0, 0.0, w, h), 0.0, Color::rgba(0.0, 0.0, 0.0, 0.33), Color::rgba(0.0, 0.0, 0.0, 0.33), || {});
        ctx.pop();
        let (cw, ch) = (520.0_f32, 300.0_f32);
        let (x, y) = (((w - cw) / 2.0).max(8.0), ((h - ch) / 2.0).max(8.0));
        rect(ctx, x - 1.0, y - 1.0, cw + 2.0, ch + 2.0, t.danger_fg(), 11.0);
        rect(ctx, x, y, cw, ch, t.bg_surface(), 10.0);
        let pad = 24.0;
        let (lx, inner) = (x + pad, cw - 2.0 * pad);
        let line = |ctx: &mut Ctx, row: f32, s: &str, c: Color| text(ctx, lx, y + row, &fit(s, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, c);
        icon(ctx, lx, y + 20.0, ph::WARNING, 20.0, t.danger_fg());
        text(ctx, lx + 28.0, y + 32.0, &fit(&format!("{} isn't the server you saved", k.saved.addr.host), type_size::LG, Weight::Semibold, inner - 28.0), type_size::LG, Weight::Semibold, t.text_primary());
        // A WebDAV share saved as trusted has no key of its own: its name
        // must still be vouched for by the trust store.
        let by_name = k.saved.addr.proto == crate::connect::Proto::Webdav && k.saved.key == [0; 32];
        if by_name {
            line(ctx, 66.0, "Its certificate is no longer one Atrium trusts, so Scrinium won't connect", t.text_secondary());
        } else if k.saved.addr.proto == crate::connect::Proto::Smb {
            line(ctx, 66.0, "A different machine answers at its address, so Scrinium won't connect", t.text_secondary());
        } else {
            line(ctx, 66.0, "It shows a different key from the one you accepted, so Scrinium won't connect", t.text_secondary());
        }
        line(ctx, 86.0, "and Arca won't give it your password. Someone may be pretending to be it.", t.text_secondary());
        line(ctx, 114.0, "You accepted", t.text_tertiary());
        rect(ctx, lx, y + 122.0, inner, 30.0, t.bg_canvas(), 6.0);
        let accepted = if by_name { "A certificate the trust store vouches for".to_string() } else { crate::connect::fingerprint(&k.saved.key) };
        text(ctx, lx + 10.0, y + 137.0, &fit(&accepted, type_size::SM, Weight::Medium, inner - 20.0), type_size::SM, Weight::Medium, t.text_primary());
        line(ctx, 170.0, "It shows now", t.text_tertiary());
        rect(ctx, lx, y + 178.0, inner, 30.0, t.bg_canvas(), 6.0);
        text(ctx, lx + 10.0, y + 193.0, &fit(&k.now, type_size::SM, Weight::Medium, inner - 20.0), type_size::SM, Weight::Medium, t.danger_fg());
        line(ctx, 226.0, "If its owner says the key really changed, forget it and connect to it again.", t.text_tertiary());
        let by = y + ch - em(32.0) - pad;
        let close = width("Close", type_size::SM, Weight::Medium) + 28.0;
        let s = self.state.clone();
        button(ctx, x + cw - pad - close, by, "Close", Look::Primary, true, move || with(&s, Browser::close_key_changed));
        let label = "Forget this server";
        let fw = width(label, type_size::SM, Weight::Medium) + 28.0;
        let s = self.state.clone();
        button(ctx, x + cw - pad - close - 8.0 - fw, by, label, Look::Ghost, true, move || with(&s, Browser::forget_changed));
    }

    /// In place of the file list: a Locked stick (§7.2 step 4) and what
    /// can be done with it, or another persona's stick.
    #[allow(clippy::too_many_arguments)]
    fn sealed_pane(&self, ctx: &mut Ctx, b: &Browser, i: usize, p: &crate::browser::Place, x0: f32, w: f32, y0: f32, y1: f32) {
        use crate::browser::persona_name;
        let t = ctx.theme;
        let cx = x0 + (w - x0) / 2.0;
        let room = (w - x0 - 48.0).max(0.0);
        let mut y = y0 + ((y1 - y0) * 0.3).min(em(140.0));
        icon(ctx, cx - 16.0, y, ph::LOCK_SIMPLE, 32.0, t.text_tertiary());
        // Under a stick sheet the sheet says it: a heading centred on the
        // pane (right of the window's centre) would show past its edge.
        if b.stick_sheet.is_some() {
            return;
        }
        y += em(56.0);
        let centred = |ctx: &mut Ctx, y: f32, s: &str, size: f32, weight: Weight, c: Color| {
            let s = fit(s, size, weight, room);
            let tw = width(&s, size, weight);
            text(ctx, cx - tw / 2.0, y, &s, size, weight, c);
        };
        if !p.seal.locked {
            let who = persona_name(&p.seal.persona);
            centred(ctx, y, &format!("This is a {who} stick"), type_size::LG, Weight::Semibold, t.text_primary());
            centred(ctx, y + em(28.0), &format!("Open it from a {who} window. Only {who} apps get its files."), type_size::SM, Weight::Regular, t.text_secondary());
            return;
        }
        let (title, line) = if p.seal.older {
            ("This stick is older than the last time it was used here", "Someone may have put back an earlier copy of it. You can open it read-only to look.")
        } else {
            ("Encrypted stick from another machine or persona", "Nothing on it can be read without its key.")
        };
        centred(ctx, y, title, type_size::LG, Weight::Semibold, t.text_primary());
        centred(ctx, y + em(28.0), line, type_size::SM, Weight::Regular, t.text_secondary());
        if b.picker().is_some() {
            return;
        }
        // Its actions, centred in a row.
        let mut acts: Vec<(&str, Look, Box<dyn Fn()>)> = Vec::new();
        let s = self.state.clone();
        if p.seal.older {
            acts.push(("Open read-only…", Look::Primary, Box::new(move || with(&s, |b| b.ask_about(i)))));
        } else if p.seal.recovery {
            acts.push(("Enter recovery code…", Look::Primary, Box::new(move || with(&s, |b| {
                b.ask_about(i);
                b.ask_recovery();
            }))));
        }
        let s = self.state.clone();
        acts.push(("Erase and format…", Look::Ghost, Box::new(move || with(&s, |b| b.ask_format(i)))));
        let total: f32 = acts.iter().map(|(l, ..)| button_w(l)).sum::<f32>() + 8.0 * (acts.len() as f32 - 1.0);
        let mut x = cx - total / 2.0;
        for (label, look, on) in acts {
            x += button(ctx, x, y + em(52.0), label, look, true, on) + 8.0;
        }
    }

    /// A sealed stick's sheet (tessera-encryption.md §7, §8).
    fn stick_sheet(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        use crate::browser::{persona_name, StickAsk};
        let t = ctx.theme;
        let Some(sh) = &b.stick_sheet else { return };
        ctx.key(4300);
        area(ctx, Rect::new(0.0, 0.0, w, h), 0.0, Color::rgba(0.0, 0.0, 0.0, 0.33), Color::rgba(0.0, 0.0, 0.0, 0.33), || {});
        ctx.pop();
        let pad = 24.0;
        let (cw, ch) = (520.0_f32.min(w - 16.0), em(236.0));
        let (x, y) = (((w - cw) / 2.0).max(8.0), ((h - ch) / 2.0).max(8.0));
        let danger = matches!(sh.ask, StickAsk::Format { .. } | StickAsk::Forget { .. } | StickAsk::RemoveCode);
        rect(ctx, x - 1.0, y - 1.0, cw + 2.0, ch + 2.0, if danger { t.danger_fg() } else { t.border_strong() }, 11.0);
        rect(ctx, x, y, cw, ch, t.bg_surface(), 10.0);
        let (lx, inner) = (x + pad, cw - 2.0 * pad);
        let title = |ctx: &mut Ctx, s: &str| text(ctx, lx, y + em(32.0), &fit(s, type_size::LG, Weight::Semibold, inner), type_size::LG, Weight::Semibold, t.text_primary());
        let line = |ctx: &mut Ctx, row: f32, s: &str, c: Color| text(ctx, lx, y + em(row), &fit(s, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, c);
        // A tick box: drawn (the UI fonts have no ballot-box glyphs).
        let tick = |ctx: &mut Ctx, row: f32, label: &str, on: bool, key: u64, press: Box<dyn Fn()>| {
            ctx.key(key);
            area(ctx, Rect::new(lx - 6.0, y + em(row) - em(14.0), inner.min(em(320.0)), em(28.0)), 6.0, Color::TRANSPARENT, t.state_hover(), press);
            rect(ctx, lx, y + em(row) - 8.0, 16.0, 16.0, t.border_strong(), 4.0);
            rect(ctx, lx + 1.0, y + em(row) - 7.0, 14.0, 14.0, if on { t.accent_fg() } else { t.bg_surface() }, 3.0);
            text(ctx, lx + 24.0, y + em(row), label, type_size::SM, Weight::Regular, t.text_primary());
            ctx.pop();
        };
        let by = y + ch - em(32.0) - pad;
        let mut right = x + cw - pad;
        let mut btn = |ctx: &mut Ctx, label: &str, look: Look, enabled: bool, on: Box<dyn Fn()>| {
            let bw = button_w(label);
            right -= bw;
            button(ctx, right, by, label, look, enabled, on);
            right -= 8.0;
        };
        let busy = sh.busy;
        let s = self.state.clone();
        let close: Box<dyn Fn()> = Box::new(move || with(&s, Browser::close_stick_sheet));
        let s = self.state.clone();
        let confirm: Box<dyn Fn()> = Box::new(move || with(&s, |b| b.confirm_stick(None)));
        match &sh.ask {
            StickAsk::Format { persona, recovery } => {
                title(ctx, &if persona == "personal" { format!("Encrypt “{}”?", sh.label) } else { format!("Make “{}” a {} stick?", sh.label, persona_name(persona)) });
                icon(ctx, lx, y + em(52.0), ph::WARNING, 18.0, t.danger_fg());
                text(ctx, lx + 26.0, y + em(62.0), &fit("Everything on it now will be erased.", type_size::SM, Weight::Medium, inner - 26.0), type_size::SM, Weight::Medium, t.danger_fg());
                let whose = if persona == "personal" { "you".to_string() } else { format!("{} apps", persona_name(persona)) };
                line(ctx, 88.0, &format!("Its key stays in Arca on this machine. Only {whose} can open it here;"), t.text_secondary());
                line(ctx, 108.0, "anywhere else it's unreadable without its key or a recovery code.", t.text_secondary());
                let s = self.state.clone();
                tick(ctx, 140.0, "Make a recovery code", *recovery, 4301, Box::new(move || with(&s, Browser::toggle_recovery)));
                if !recovery && persona == "personal" {
                    line(ctx, 164.0, "Without one, losing this machine loses the stick's files.", t.text_tertiary());
                } else if *recovery && persona != "personal" {
                    line(ctx, 164.0, "While a code exists, forgetting the key here doesn't lock the stick.", t.text_tertiary());
                }
                btn(ctx, if busy { "Erasing…" } else { "Erase and encrypt" }, Look::Danger, !busy, confirm);
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
            StickAsk::Code { code } => {
                title(ctx, "Write down this recovery code");
                line(ctx, 66.0, "It opens the stick on any machine. Scrinium shows it only this once.", t.text_secondary());
                rect(ctx, lx, y + em(84.0), inner, em(44.0), t.bg_canvas(), 6.0);
                // Never cut: smaller type before that.
                let mut style = TextStyle { family: font::MONO.into(), size: type_size::LG, weight: Weight::Medium, color: t.text_primary() };
                for size in [type_size::MD, type_size::SM, type_size::XS] {
                    if Node::measure_text(code, &style).w <= inner - 20.0 {
                        break;
                    }
                    style.size = size;
                }
                ctx.add(Node::Text { rect: Rect::new(lx + 10.0, y + em(84.0), 0.0, em(44.0)), content: code.clone(), style });
                line(ctx, 150.0, "Keep it somewhere other than with the stick.", t.text_tertiary());
                btn(ctx, "I've written it down", Look::Primary, true, close);
            }
            StickAsk::Locked => {
                title(ctx, &format!("“{}” is locked", sh.label));
                line(ctx, 66.0, "Encrypted stick from another machine or persona.", t.text_secondary());
                btn(ctx, "Close", Look::Ghost, true, close);
            }
            StickAsk::Recovery { keep } => {
                title(ctx, &format!("Recovery code for “{}”", sh.label));
                line(ctx, 66.0, "Type it into Arca's field: Scrinium never sees it.", t.text_secondary());
                self.rcode.clone().at(lx, y + em(84.0)).size(inner, 36.0).render(ctx);
                let s = self.state.clone();
                let who = persona_name(&b.persona);
                tick(ctx, 146.0, &format!("Keep its key in {who} on this machine"), *keep, 4302, Box::new(move || with(&s, Browser::toggle_keep)));
                if !keep {
                    line(ctx, 170.0, "It opens for this session only; the code is needed again next time.", t.text_tertiary());
                }
                let (s, st) = (self.state.clone(), self.rcode_state.clone());
                btn(ctx, if busy { "Opening…" } else { "Open" }, Look::Primary, !busy, Box::new(move || {
                    let h = st.get_cloned().handle;
                    with(&s, |b| b.confirm_stick(h));
                }));
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
            StickAsk::Older => {
                title(ctx, "Open an older copy read-only?");
                line(ctx, 66.0, &format!("“{}” is older than the last time it was used here.", sh.label), t.text_secondary());
                line(ctx, 86.0, "Restoring a backup does this; so could someone putting back an old copy.", t.text_secondary());
                line(ctx, 106.0, "You'll be asked to confirm it's you. Nothing on it can be changed.", t.text_tertiary());
                btn(ctx, if busy { "Waiting for you…" } else { "Open read-only" }, Look::Primary, !busy, confirm);
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
            StickAsk::MakeCurrent => {
                title(ctx, "Use this copy as current?");
                line(ctx, 66.0, &format!("“{}” becomes writable, and this machine forgets that it", sh.label), t.text_secondary());
                line(ctx, 86.0, "saw a newer copy: the newer copy won't open here afterwards.", t.text_secondary());
                line(ctx, 106.0, "You'll be asked to confirm it's you.", t.text_tertiary());
                btn(ctx, if busy { "Waiting for you…" } else { "Use as current" }, Look::Primary, !busy, confirm);
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
            StickAsk::Forget { recovery } => {
                title(ctx, &format!("Forget the key of “{}”?", sh.label));
                line(ctx, 66.0, "This machine won't be able to open it again: it will show Locked.", t.text_secondary());
                if *recovery {
                    line(ctx, 86.0, "Its recovery code still opens it, anywhere. To make it unreadable,", t.text_secondary());
                    line(ctx, 106.0, "erase it instead (Erase and format, once it's Locked).", t.text_secondary());
                } else {
                    line(ctx, 86.0, "It has no recovery code, so nothing on it can be read again.", t.text_secondary());
                }
                line(ctx, 126.0, "You'll be asked to confirm it's you.", t.text_tertiary());
                btn(ctx, if busy { "Waiting for you…" } else { "Forget key" }, Look::Danger, !busy, confirm);
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
            StickAsk::RemoveCode => {
                title(ctx, &format!("Remove the recovery code of “{}”?", sh.label));
                line(ctx, 66.0, "The code you wrote down won't open it any more, here or anywhere.", t.text_secondary());
                line(ctx, 86.0, "Only this machine's key will: if it's lost, so is everything on it.", t.text_secondary());
                btn(ctx, if busy { "Removing…" } else { "Remove code" }, Look::Danger, !busy, confirm);
                btn(ctx, "Cancel", Look::Ghost, !busy, close);
            }
        }
        if let Some(e) = &sh.error {
            text(ctx, lx, by - em(14.0), &fit(e, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.danger_fg());
        }
    }

    /// Opening a file (opening-files.md §6.1, D3): Open with, or the
    /// question when its name and its bytes disagree.
    fn open_sheet(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        let Some(sheet) = &b.open_sheet else { return };
        ctx.key(4200);
        area(ctx, Rect::new(0.0, 0.0, w, h), 0.0, Color::rgba(0.0, 0.0, 0.0, 0.33), Color::rgba(0.0, 0.0, 0.0, 0.33), || {});
        ctx.pop();
        let pad = 24.0;
        let row = em(44.0);
        let (cw, ch) = match sheet {
            OpenSheet::Choose { apps, .. } => (480.0_f32, em(150.0) + row * apps.len().min(6) as f32),
            OpenSheet::Question { .. } => (520.0_f32, em(170.0)),
        };
        let (x, y) = (((w - cw) / 2.0).max(8.0), ((h - ch) / 2.0).max(8.0));
        rect(ctx, x - 1.0, y - 1.0, cw + 2.0, ch + 2.0, t.border_strong(), 11.0);
        rect(ctx, x, y, cw, ch, t.bg_surface(), 10.0);
        let (lx, inner) = (x + pad, cw - 2.0 * pad);
        let by = y + ch - em(32.0) - pad;
        let cancel = width("Cancel", type_size::SM, Weight::Medium) + 28.0;
        let s = self.state.clone();
        button(ctx, x + cw - pad - cancel, by, "Cancel", Look::Ghost, true, move || with(&s, Browser::close_open_sheet));
        match sheet {
            OpenSheet::Choose { req, words, apps, always } => {
                let title = format!("Open “{}” with", req.name);
                text(ctx, lx, y + em(32.0), &fit(&title, type_size::LG, Weight::Semibold, inner), type_size::LG, Weight::Semibold, t.text_primary());
                text(ctx, lx, y + em(56.0), &fit(&format!("Apps that open {words}"), type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
                let mut ry = y + em(72.0);
                for (i, c) in apps.iter().take(6).enumerate() {
                    let s = self.state.clone();
                    ctx.key(4300 + i as u64);
                    area(ctx, Rect::new(lx - 8.0, ry, inner + 16.0, row - 4.0), 6.0, t.bg_canvas(), t.state_hover(), move || with(&s, |b| b.choose_app(i)));
                    text(ctx, lx + 4.0, ry + (row - 4.0) / 2.0, &fit(&c.name, type_size::MD, Weight::Medium, inner - 140.0), type_size::MD, Weight::Medium, t.text_primary());
                    let tag = match (c.default, c.edit) {
                        (true, true) => "Default · opens and changes",
                        (true, false) => "Default · opens to view",
                        (false, true) => "Opens and changes",
                        (false, false) => "Opens to view",
                    };
                    let tw = width(tag, type_size::XS, Weight::Regular);
                    text(ctx, lx + inner - tw, ry + (row - 4.0) / 2.0, tag, type_size::XS, Weight::Regular, t.text_tertiary());
                    ctx.pop();
                    ry += row;
                }
                // "Always", D7: Scrinium's sheet is one of the two places a
                // default is changed.
                let label = format!("Always open {words} with the app I pick");
                let lw = width(&label, type_size::SM, Weight::Regular) + 22.0;
                let s = self.state.clone();
                let cy = by + em(16.0);
                ctx.key(4290);
                area(ctx, Rect::new(lx - 4.0, cy - em(14.0), lw + 8.0, em(28.0)), 4.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, Browser::toggle_always));
                rect(ctx, lx, cy - 7.0, 14.0, 14.0, t.border_strong(), 3.0);
                rect(ctx, lx + 1.0, cy - 6.0, 12.0, 12.0, if *always { t.accent_fg() } else { t.bg_surface() }, 2.0);
                text(ctx, lx + 22.0, cy, &fit(&label, type_size::SM, Weight::Regular, inner - cancel - 30.0), type_size::SM, Weight::Regular, t.text_secondary());
                ctx.pop();
            }
            OpenSheet::Question { req, says, actual, text: as_text } => {
                icon(ctx, lx, y + 20.0, ph::WARNING, 20.0, t.warning_fg());
                text(ctx, lx + 28.0, y + 32.0, &fit(&format!("“{}”", req.name), type_size::LG, Weight::Semibold, inner - 28.0), type_size::LG, Weight::Semibold, t.text_primary());
                text(ctx, lx, y + em(64.0), &fit(says, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, lx, y + em(86.0), &fit("Open it only if you know where it came from.", type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_tertiary());
                let mut bx = x + cw - pad - cancel - 8.0;
                if *as_text {
                    let l = "Open as text";
                    let bw = width(l, type_size::SM, Weight::Medium) + 28.0;
                    bx -= bw;
                    let s = self.state.clone();
                    button(ctx, bx, by, l, if actual.is_some() { Look::Secondary } else { Look::Primary }, true, move || with(&s, |b| b.open_as("text/plain")));
                    bx -= 8.0;
                }
                if let Some((mime, words)) = actual {
                    let l = format!("Open as {words}");
                    let bw = width(&l, type_size::SM, Weight::Medium) + 28.0;
                    bx -= bw;
                    let (s, m) = (self.state.clone(), mime.clone());
                    button(ctx, bx, by, &l, Look::Primary, true, move || with(&s, |b| b.open_as(&m)));
                }
            }
        }
    }

    /// "Connect to server", over the window (network-shares.md §5.2).
    fn connect_sheet(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        let Some(form) = &b.connect else { return };
        // A scrim that takes presses, so nothing behind reacts.
        ctx.key(4000);
        area(ctx, Rect::new(0.0, 0.0, w, h), 0.0, Color::rgba(0.0, 0.0, 0.0, 0.33), Color::rgba(0.0, 0.0, 0.0, 0.33), || {});
        ctx.pop();
        let (cw, ch) = (480.0_f32, 268.0_f32);
        let (x, y) = (((w - cw) / 2.0).max(8.0), ((h - ch) / 2.0).max(8.0));
        rect(ctx, x - 1.0, y - 1.0, cw + 2.0, ch + 2.0, t.border_strong(), 11.0);
        rect(ctx, x, y, cw, ch, t.bg_surface(), 10.0);
        let pad = 24.0;
        let (lx, inner) = (x + pad, cw - 2.0 * pad);
        let title = |ctx: &mut Ctx, s: &str| text(ctx, lx, y + 32.0, s, type_size::LG, Weight::Semibold, t.text_primary());
        let line = |ctx: &mut Ctx, row: f32, s: &str, c: Color| text(ctx, lx, y + row, &fit(s, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, c);
        let by = y + ch - em(32.0) - pad;
        let cancel = |ctx: &mut Ctx, s: Mutable<Browser>| {
            let cw2 = width("Cancel", type_size::SM, Weight::Medium) + 28.0;
            button(ctx, x + cw - pad - cw2, by, "Cancel", Look::Ghost, true, move || with(&s, Browser::close_connect));
            cw2
        };
        let primary = |ctx: &mut Ctx, label: &str, left_of: f32, enabled: bool, on: Box<dyn Fn()>| {
            let pw = width(label, type_size::SM, Weight::Medium) + 28.0;
            button(ctx, x + cw - pad - left_of - 8.0 - pw, by, label, Look::Primary, enabled, move || on());
        };
        match &form.step {
            Step::Address => {
                title(ctx, "Connect to server");
                line(ctx, 66.0, "Type the server's address with your name on it, and a folder if you like:", t.text_secondary());
                line(ctx, 88.0, "alice@nas.local:/srv  (SFTP),  https://alice@nas.local/dav,  smb://alice@nas.local/photos", t.text_tertiary());
                let fy = y + 108.0;
                let s = self.state.clone();
                let holder = ctx.push(Node::Rect { rect: Rect::new(lx, fy, inner, 32.0), fill: Color::TRANSPARENT, radius: 0.0 });
                TextField::new(self.field.clone()).at(lx, fy).width(inner).on_submit(move |v| with(&s, |b| if let Some(f) = &mut b.connect { f.submit_address(v) })).render(ctx);
                ctx.pop();
                if let Some(&bg) = ctx.tree.children_of(holder).get(2) {
                    ctx.autofocus(bg);
                }
                if let Some(e) = &form.error {
                    line(ctx, 162.0, e, t.danger_fg());
                }
                let c = cancel(ctx, self.state.clone());
                let (s, f) = (self.state.clone(), self.field.clone());
                primary(ctx, "Next", c, true, Box::new(move || {
                    let v = f.get_cloned().text().to_string();
                    with(&s, |b| if let Some(f) = &mut b.connect { f.submit_address(&v) });
                }));
            }
            Step::Checking(a) => {
                title(ctx, "Connect to server");
                line(ctx, 72.0, &format!("Asking {} who it is…", a.host), t.text_secondary());
                cancel(ctx, self.state.clone());
            }
            Step::Key { addr, fingerprint } if addr.proto == crate::connect::Proto::Webdav => {
                // A certificate the trust store doesn't vouch for: pinned by
                // its key, once the user has checked it.
                let c = form.cert.clone().unwrap_or_default();
                icon(ctx, lx, y + 20.0, ph::WARNING, 20.0, t.text_secondary());
                text(ctx, lx + 28.0, y + 32.0, &format!("Is this really {}?", addr.host), type_size::LG, Weight::Semibold, t.text_primary());
                line(ctx, 62.0, &crate::connect::untrusted_text(c.untrusted.as_deref().unwrap_or("")), t.text_secondary());
                line(ctx, 82.0, &format!("Certificate for “{}”, from “{}”, until {}", c.subject, c.issuer, c.expires), t.text_tertiary());
                line(ctx, 102.0, "If it is your own server, check its key is this one:", t.text_secondary());
                rect(ctx, lx, y + 112.0, inner, 32.0, t.bg_canvas(), 6.0);
                text(ctx, lx + 10.0, y + 128.0, &fit(fingerprint, type_size::SM, Weight::Medium, inner - 20.0), type_size::SM, Weight::Medium, t.text_primary());
                line(ctx, 164.0, "Scrinium remembers it, and refuses the server if it ever changes.", t.text_tertiary());
                let c = cancel(ctx, self.state.clone());
                let s = self.state.clone();
                primary(ctx, "It's the right key", c, true, Box::new(move || with(&s, |b| if let Some(f) = &mut b.connect { f.trust() })));
            }
            Step::Key { addr, .. } if addr.proto == crate::connect::Proto::Smb => {
                // A password server can't prove who it is (§5.3): say so,
                // and remember it by its ID.
                let i = form.smb.clone().unwrap_or_default();
                icon(ctx, lx, y + 20.0, ph::WARNING, 20.0, t.text_secondary());
                text(ctx, lx + 28.0, y + 32.0, &format!("Connect to {}?", addr.host), type_size::LG, Weight::Semibold, t.text_primary());
                line(ctx, 62.0, &format!("It speaks SMB 3.1.1 and everything is encrypted ({}).", i.cipher), t.text_secondary());
                line(ctx, 82.0, "A password server can't prove who it is. Scrinium remembers this one", t.text_secondary());
                line(ctx, 102.0, "by its ID and won't connect if a different machine answers:", t.text_secondary());
                rect(ctx, lx, y + 112.0, inner, 32.0, t.bg_canvas(), 6.0);
                text(ctx, lx + 10.0, y + 128.0, &fit(&format!("Server ID {}", i.guid), type_size::SM, Weight::Medium, inner - 20.0), type_size::SM, Weight::Medium, t.text_primary());
                line(ctx, 164.0, "Someone on this network could try to guess the password:", t.danger_fg());
                line(ctx, 184.0, "use one you use nowhere else.", t.danger_fg());
                let c = cancel(ctx, self.state.clone());
                let s = self.state.clone();
                primary(ctx, "Connect to it", c, true, Box::new(move || with(&s, |b| if let Some(f) = &mut b.connect { f.trust() })));
            }
            Step::Key { addr, fingerprint } => {
                icon(ctx, lx, y + 20.0, ph::WARNING, 20.0, t.text_secondary());
                text(ctx, lx + 28.0, y + 32.0, &format!("Is this really {}?", addr.host), type_size::LG, Weight::Semibold, t.text_primary());
                line(ctx, 66.0, "The server shows this key. Check that it is the one the server's owner", t.text_secondary());
                line(ctx, 86.0, "gives you. If it isn't, someone may be pretending to be the server.", t.text_secondary());
                rect(ctx, lx, y + 106.0, inner, 36.0, t.bg_canvas(), 6.0);
                text(ctx, lx + 10.0, y + 124.0, &fit(fingerprint, type_size::SM, Weight::Medium, inner - 20.0), type_size::SM, Weight::Medium, t.text_primary());
                line(ctx, 160.0, "Scrinium remembers it, and refuses the server if it ever changes.", t.text_tertiary());
                let c = cancel(ctx, self.state.clone());
                let s = self.state.clone();
                primary(ctx, "It's the right key", c, true, Box::new(move || with(&s, |b| if let Some(f) = &mut b.connect { f.trust() })));
            }
            Step::Password { addr, fingerprint, .. } => {
                title(ctx, &format!("Password for {}@{}", addr.user, addr.host));
                match form.cert.as_ref().filter(|_| fingerprint == crate::connect::TRUSTED) {
                    Some(c) => line(ctx, 66.0, &format!("Its certificate checks out (from “{}”). Arca keeps the password: Scrinium never sees it.", c.issuer), t.text_secondary()),
                    None => line(ctx, 66.0, "Arca keeps it: Scrinium never sees what you type.", t.text_secondary()),
                };
                self.cred.clone().at(lx, y + 88.0).size(inner, 36.0).render(ctx);
                let s = self.state.clone();
                ctx.key(4001);
                area(ctx, Rect::new(lx - 6.0, y + 136.0, 240.0, 28.0), 6.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| if let Some(f) = &mut b.connect { f.remember = !f.remember }));
                // A drawn box: the UI fonts have no ballot-box glyphs.
                rect(ctx, lx, y + 142.0, 16.0, 16.0, t.border_strong(), 4.0);
                // Ticked: a solid box (a hollow one read as empty).
                rect(ctx, lx + 1.0, y + 143.0, 14.0, 14.0, if form.remember { t.accent_fg() } else { t.bg_surface() }, 3.0);
                text(ctx, lx + 24.0, y + 150.0, "Remember it in Arca", type_size::SM, Weight::Regular, t.text_primary());
                ctx.pop();
                // Connect at login (D8): off unless ticked, and only for a
                // share Arca remembers.
                let s = self.state.clone();
                ctx.key(4002);
                area(ctx, Rect::new(lx - 6.0, y + 166.0, 240.0, 28.0), 6.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| if let Some(f) = &mut b.connect { if f.remember { f.at_login = !f.at_login } }));
                let on = form.remember && form.at_login;
                rect(ctx, lx, y + 172.0, 16.0, 16.0, t.border_strong(), 4.0);
                rect(ctx, lx + 1.0, y + 173.0, 14.0, 14.0, if on { t.accent_fg() } else { t.bg_surface() }, 3.0);
                text(ctx, lx + 24.0, y + 180.0, "Connect when I log in", type_size::SM, Weight::Regular, if form.remember { t.text_primary() } else { t.text_tertiary() });
                ctx.pop();
                if let Some(e) = &form.error {
                    line(ctx, 210.0, e, t.danger_fg());
                }
                let c = cancel(ctx, self.state.clone());
                let s = self.state.clone();
                primary(ctx, "Connect", c, form.can_press_connect(), Box::new(move || with(&s, |b| if let Some(f) = &mut b.connect { f.connect() })));
            }
            Step::Connecting(a) => {
                title(ctx, "Connect to server");
                line(ctx, 72.0, &format!("Connecting to {}…", a.host), t.text_secondary());
            }
        }
    }

    /// The path as crumbs from `x` on the toolbar row; each goes up to it.
    fn crumbs(&self, ctx: &mut Ctx, b: &Browser, x: f32, cy: f32, max_x: f32) {
        let t = ctx.theme;
        let Some(p) = b.place() else { return };
        let mut names: Vec<String> = vec![p.label.clone()];
        names.extend(b.path.iter().cloned());
        // Too long: keep the last crumbs, with an ellipsis for the rest.
        let mut first = 0;
        let total = |from: usize| names[from..].iter().map(|n| width(n, type_size::SM, Weight::Medium) + 10.0 + em(18.0)).sum::<f32>();
        while first + 1 < names.len() && x + total(first) > max_x {
            first += 1;
        }
        let mut cx = x;
        if first > 0 {
            text(ctx, cx, cy, "…", type_size::SM, Weight::Regular, t.text_tertiary());
            cx += width("…", type_size::SM, Weight::Regular) + 4.0;
            icon(ctx, cx, cy - em(7.0), ph::CARET_RIGHT, 14.0, t.text_tertiary());
            cx += em(18.0);
        }
        for (d, n) in names.iter().enumerate().skip(first) {
            let last = d + 1 == names.len();
            let label = fit(n, type_size::SM, Weight::Medium, (max_x - cx - 10.0).max(24.0));
            let wd = width(&label, type_size::SM, Weight::Medium) + 10.0;
            let s = self.state.clone();
            ctx.key(2000 + d as u64);
            area(ctx, Rect::new(cx - 5.0, cy - em(13.0), wd, em(26.0)), 5.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| b.up_to(d)));
            text(ctx, cx, cy, &label, type_size::SM, if last { Weight::Semibold } else { Weight::Medium }, if last { t.text_primary() } else { t.text_secondary() });
            ctx.pop();
            cx += wd;
            if !last {
                icon(ctx, cx, cy - em(7.0), ph::CARET_RIGHT, 14.0, t.text_tertiary());
                cx += em(18.0);
            }
        }
    }

    /// The list's column heads and its rows, in `x0..x1`, `y0..y1`.
    fn list(&self, ctx: &mut Ctx, b: &Browser, x0: f32, x1: f32, y0: f32, y1: f32, wide: bool) {
        let t = ctx.theme;
        let lw = x1 - x0;
        // Columns, right to left: modified, size, kind.
        let (mod_w, size_w, kind_w) = if wide { (em(140.0), em(90.0), em(100.0)) } else { (0.0, em(84.0), 0.0) };
        let size_x = lw - mod_w - size_w - 12.0;
        let kind_x = size_x - kind_w;
        rect(ctx, x0, y0, lw, header_h(), t.bg_canvas(), 0.0);
        rect(ctx, x0, y0 + header_h() - 1.0, lw, 1.0, t.border_default(), 0.0);
        let head = |ctx: &mut Ctx, x: f32, s: &str| {
            text(ctx, x0 + x, y0 + header_h() / 2.0, s, type_size::XS, Weight::Semibold, t.text_tertiary());
        };
        head(ctx, text_x(), "Name");
        if wide {
            head(ctx, kind_x, "Kind");
            head(ctx, size_x + size_w - width("Size", type_size::XS, Weight::Semibold), "Size");
            head(ctx, lw - mod_w, "Modified");
        } else {
            head(ctx, size_x + size_w - width("Size", type_size::XS, Weight::Semibold), "Size");
        }
        let top = y0 + header_h();
        let lh = (y1 - top).max(0.0);
        if b.entries.is_empty() {
            let msg = match (&b.unreadable, b.place()) {
                (Some(why), _) => why.clone(),
                (None, None) => "Nothing to show".into(),
                (None, Some(_)) if b.picker().is_some_and(|f| f.kind == Kind::Folder) => "No folders here".into(),
                (None, Some(_)) if b.picker().is_some_and(|f| f.kind == Kind::Save) => "This folder is empty: save here".into(),
                (None, Some(_)) => "This folder is empty".into(),
            };
            let tw = width(&msg, type_size::SM, Weight::Regular);
            text(ctx, x0 + (lw - tw) / 2.0, top + lh.min(200.0) / 2.0, &msg, type_size::SM, Weight::Regular, t.text_tertiary());
            return;
        }
        {
            let mut shown = self.shown_cursor.lock().unwrap();
            if b.cursor != *shown {
                *shown = b.cursor;
                if let Some(i) = b.cursor {
                    VirtualList::reveal(&self.scroll, i, b.entries.len(), row_h(), lh);
                }
            }
        }
        let entries = b.entries.clone();
        let selected = b.selected.clone();
        let handlers: Vec<_> = (0..entries.len()).map(|i| std::rc::Rc::new(self.press_handler(i))).collect();
        let picking_files = b.picker().map(|f| matches!(f.kind, Kind::File | Kind::Workbook));
        VirtualList::new(self.scroll.clone(), Rect::new(x0, top, lw, lh), entries.len(), row_h(), move |i, ctx| {
            let t = ctx.theme;
            let e = &entries[i];
            let sel = selected.binary_search(&i).is_ok();
            let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, lw, row_h()), 0.0));
            ctx.set_flex(row, FlexStyle { free: true, ..FlexStyle::default() });
            let h = handlers[i].clone();
            area(ctx, Rect::new(6.0, 1.0, lw - 12.0, row_h() - 2.0), 5.0, if sel { t.selection_bg() } else { Color::TRANSPARENT }, if sel { t.selection_bg() } else { t.state_hover() }, move || h());
            // In a file picker folders are for going into: dimmer.
            let dim = picking_files == Some(true) && e.dir;
            let fg = if e.link { t.text_tertiary() } else { t.text_primary() };
            icon(ctx, 16.0, em(7.0), file_icon(&e.name, e.dir, e.link), 16.0, if e.dir { t.accent_fg() } else { t.text_secondary() });
            let name_w = if kind_w > 0.0 { kind_x } else { size_x } - text_x() - 8.0;
            text(ctx, text_x(), row_h() / 2.0, &fit(&e.name, type_size::SM, Weight::Regular, name_w), type_size::SM, Weight::Regular, if dim { t.text_secondary() } else { fg });
            if kind_w > 0.0 {
                text(ctx, kind_x, row_h() / 2.0, fs::kind_of(&e.name, e.dir), type_size::SM, Weight::Regular, t.text_secondary());
            }
            if !e.dir {
                let s = fs::size_text(e.size);
                let sw = width(&s, type_size::SM, Weight::Regular);
                text(ctx, size_x + size_w - sw, row_h() / 2.0, &s, type_size::SM, Weight::Regular, t.text_secondary());
            }
            if mod_w > 0.0 {
                if let Some(m) = e.modified {
                    text(ctx, lw - mod_w, row_h() / 2.0, &fs::time_text(m), type_size::SM, Weight::Regular, t.text_secondary());
                }
            }
            ctx.pop();
            ctx.pop();
        })
        .key(77)
        .backdrop(ctx.theme.bg_canvas())
        .emit(ctx);
    }

    /// The quick look pane: the file's name, kind and size, then its text
    /// (drawn here, never interpreted), its viewer's slot, or why there is
    /// nothing to show.
    fn look_pane(&self, ctx: &mut Ctx, l: &crate::browser::Look, x0: f32, y0: f32, x1: f32, y1: f32) {
        use crate::browser::LookBody;
        let t = ctx.theme;
        let (w, pad) = (x1 - x0, em(14.0));
        rect(ctx, x0, y0, w, y1 - y0, t.bg_surface(), 0.0);
        rect(ctx, x0, y0, 1.0, y1 - y0, t.border_default(), 0.0);
        let inner = (w - 2.0 * pad).max(0.0);
        let title_width = if l.can_print() { (inner - button_w("Print…") - pad).max(0.0) } else { inner };
        text(ctx, x0 + pad, y0 + em(22.0), &fit(&l.name, type_size::MD, Weight::Medium, title_width), type_size::MD, Weight::Medium, t.text_primary());
        if l.can_print() {
            let state = self.state.clone();
            let path = l.path.clone();
            button(ctx, x1 - pad - button_w("Print…"), y0 + em(8.0), "Print…", Look::Ghost, true, move || {
                with(&state, |b| b.want_look_print = Some(path.clone()));
            });
        }
        let words = match &l.body {
            Some(LookBody::Text { words, .. } | LookBody::Embed { words, .. } | LookBody::Info { words, .. }) => words.clone(),
            None => "Reading…".into(),
        };
        let sub = format!("{words} · {}", fs::size_text(l.size));
        text(ctx, x0 + pad, y0 + em(44.0), &fit(&sub, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
        let by = y0 + em(62.0);
        rect(ctx, x0 + pad, by, inner, 1.0, t.border_default(), 0.0);
        let (top, bottom) = (by + em(10.0), y1 - pad);
        match &l.body {
            Some(LookBody::Text { text: body, cut, .. }) => {
                let lh = em(18.0);
                let style = TextStyle { family: font::MONO.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_primary() };
                let room = ((bottom - top) / lh).floor().max(0.0) as usize;
                let lines: Vec<&str> = body.lines().collect();
                let shown = lines.len().min(room.saturating_sub(usize::from(*cut || lines.len() > room)));
                for (i, line) in lines.iter().take(shown).enumerate() {
                    let line = line.replace('\t', "    ");
                    let fitted = fit_mono(&line, &style, inner);
                    ctx.add(Node::Text { rect: Rect::new(x0 + pad, top + i as f32 * lh, 0.0, lh), content: fitted, style: style.clone() });
                }
                if *cut || lines.len() > shown {
                    text(ctx, x0 + pad, top + shown as f32 * lh + lh / 2.0, "…", type_size::SM, Weight::Regular, t.text_tertiary());
                }
            }
            Some(LookBody::Embed { .. }) => {
                let id = ctx.add(Node::Rect { rect: Rect::new(x0 + pad, top, inner, (bottom - top).max(0.0)), fill: t.bg_canvas(), radius: 6.0 });
                ctx.slot(id, LOOK_SLOT);
            }
            Some(LookBody::Info { why, .. }) => {
                icon(ctx, x0 + pad, top + em(4.0), ph::FILE_TEXT, 32.0, t.text_tertiary());
                if let Some(why) = why {
                    text(ctx, x0 + pad, top + em(56.0), &fit(why, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
                }
            }
            None => {}
        }
    }

    fn manager(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        let x0 = side_for(w);
        let slot = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
        ctx.slot(slot, PRINT_SLOT);
        self.sidebar(ctx, b, 0.0, x0, h);
        let compact = compact(h);
        // Toolbar: back, up, the path.
        rect(ctx, x0, toolbar_h() - 1.0, w - x0, 1.0, t.border_default(), 0.0);
        let s = self.state.clone();
        icon_button(ctx, x0 + 10.0, em(10.0), ph::ARROW_LEFT, b.can_go_back(), "Back (Alt-Left)", move || with(&s, Browser::go_back));
        let s = self.state.clone();
        icon_button(ctx, x0 + 10.0 + em(32.0), em(10.0), ph::ARROW_UP, !b.path.is_empty(), "Up (Backspace)", move || with(&s, Browser::up));
        let cx = x0 + 20.0 + em(64.0);
        // Short: the path keeps two fifths of the row, the actions the rest.
        let crumbs_end = if compact { cx + (w - 16.0 - cx) * 0.4 } else { w - 16.0 };
        self.crumbs(ctx, b, cx, toolbar_h() / 2.0, crumbs_end);

        // What can be done here.
        let ay = if compact { 4.0 } else { toolbar_h() + 4.0 };
        // A Locked stick, or another persona's, has no folder to act in.
        let open = b.dir().is_some() && b.unreadable.is_none();
        let writable = open && !b.place().is_some_and(|p| p.read_only);
        let n = b.selected.len();
        // A toolbar: when the text grows or the window narrows, what no
        // longer fits goes into its "More" menu (resolution-independence.md §6.3).
        let hid = if b.show_hidden { "Hide hidden files" } else { "Show hidden files" };
        let hw = width(hid, type_size::SM, Weight::Medium) + em(28.0);
        let (s1, f1, s2, f2) = (self.state.clone(), self.field.clone(), self.state.clone(), self.field.clone());
        let (s3, s4, s5, s6) = (self.state.clone(), self.state.clone(), self.state.clone(), self.state.clone());
        let (s7, s8, s9) = (self.state.clone(), self.state.clone(), self.state.clone());
        let one_file = n == 1 && b.entries.get(b.selected[0]).is_some_and(|e| !e.dir && !e.link);
        let mut items = vec![
            ToolItem::new("Open", move || with(&s7, |b| b.open_selected(false))).priority(1).enabled(open && one_file),
            ToolItem::new("Open with…", move || with(&s8, |b| b.open_selected(true))).priority(2).enabled(open && one_file),
            ToolItem::new("New folder", move || begin_edit(&s1, &f1, false)).priority(2).enabled(writable),
            ToolItem::new("Rename", move || begin_edit(&s2, &f2, true)).priority(1).enabled(writable && n == 1),
            ToolItem::new("Delete", move || with(&s3, Browser::ask_delete)).priority(1).enabled(writable && n > 0),
            ToolItem::new("Copy", move || with(&s4, |b| b.copy(false))).priority(3).enabled(open && n > 0),
            ToolItem::new("Cut", move || with(&s5, |b| b.copy(true))).priority(2).enabled(writable && n > 0),
            ToolItem::new("Paste", move || with(&s6, Browser::paste)).priority(3).enabled(writable && b.clip.is_some()),
        ];
        // A USB stick (not a share) can be sealed for this persona
        // (tessera-encryption.md §7.1); first into More.
        if let Some(i) = b.at.filter(|_| b.place().is_some_and(|p| p.stick && !p.network)) {
            let s = self.state.clone();
            items.push(ToolItem::new(&b.format_words(), move || with(&s, |b| b.ask_format(i))).priority(0).enabled(b.stick_sheet.is_none()));
        }
        // An open sealed stick of this persona's: its key can be forgotten (§9).
        if let Some(i) = b.at.filter(|_| b.place().is_some_and(|p| Browser::forgettable(p, &b.persona))) {
            let s = self.state.clone();
            items.push(ToolItem::new("Forget this stick's key…", move || with(&s, |b| b.ask_forget(i))).priority(0).enabled(b.stick_sheet.is_none()));
        }
        if let Some(i) = b.at.filter(|_| b.place().is_some_and(|p| Browser::code_removable(p, &b.persona))) {
            let s = self.state.clone();
            items.push(ToolItem::new("Remove recovery code…", move || with(&s, |b| b.ask_remove_code(i))).priority(0).enabled(b.stick_sheet.is_none()));
        }
        if compact {
            // The hidden-files switch joins them, first into More.
            items.push(ToolItem::new(hid, move || with(&s9, Browser::toggle_hidden)).priority(0).enabled(open));
            let tx = crumbs_end + 8.0;
            Toolbar::new(Rect::new(tx, ay, (w - 8.0 - tx).max(0.0), toolbar_h() - 8.0), self.more.clone(), items).render(ctx);
        } else {
            let tx = x0 + 8.0;
            Toolbar::new(Rect::new(tx, ay, (w - hw - 16.0 - tx).max(0.0), actions_h() - 8.0), self.more.clone(), items).render(ctx);
            let s = self.state.clone();
            button(ctx, w - hw - 8.0, ay + ((actions_h() - 8.0 - em(32.0)) / 2.0).max(0.0), hid, Look::Ghost, open, move || with(&s, Browser::toggle_hidden));
        }
        let mut y = if compact { toolbar_h() } else { toolbar_h() + actions_h() };
        rect(ctx, x0, y - 1.0, w - x0, 1.0, t.border_default(), 0.0);

        // A name being typed, or a question.
        if let Some(edit) = &b.edit {
            rect(ctx, x0, y, w - x0, strip_h(), t.accent_bg(), 0.0);
            let label = match edit {
                Edit::NewFolder => "New folder:".to_string(),
                Edit::Rename(n) => format!("Rename “{}” to:", fit(n, type_size::SM, Weight::Medium, 160.0)),
            };
            text(ctx, x0 + 16.0, y + strip_h() / 2.0, &label, type_size::SM, Weight::Medium, t.text_primary());
            let go = if matches!(edit, Edit::NewFolder) { "Create" } else { "Rename" };
            // The buttons keep their room at the strip's end; the field
            // takes what is left, at least 120 pt.
            let buttons = 10.0 + button_w(go) + 6.0 + button_w("Cancel") + 12.0;
            let fx = x0 + 24.0 + width(&label, type_size::SM, Weight::Medium);
            let fx = fx.min(w - buttons - 120.0).max(x0 + 16.0);
            let fw = (w - fx - buttons).clamp(120.0, 320.0);
            let s = self.state.clone();
            let holder = ctx.push(Node::Rect { rect: Rect::new(fx, y + em(8.0), fw, em(32.0)), fill: Color::TRANSPARENT, radius: 0.0 });
            TextField::new(self.field.clone()).at(fx, y + em(8.0)).width(fw).on_submit(move |v| with(&s, |b| b.commit_edit(v))).render(ctx);
            ctx.pop();
            if let Some(&bg) = ctx.tree.children_of(holder).get(2) {
                ctx.autofocus(bg);
            }
            let (s, f) = (self.state.clone(), self.field.clone());
            let bx = fx + fw + 10.0;
            let bw = button(ctx, bx, y + em(8.0), go, Look::Primary, true, move || {
                let v = f.get_cloned().text().to_string();
                with(&s, |b| b.commit_edit(&v));
            });
            let s = self.state.clone();
            button(ctx, bx + bw + 6.0, y + em(8.0), "Cancel", Look::Ghost, true, move || with(&s, Browser::cancel_edit));
            y += strip_h();
        } else if let Some(names) = &b.confirm_delete {
            rect(ctx, x0, y, w - x0, strip_h(), t.accent_bg(), 0.0);
            // The buttons keep their room at the strip's end; the question
            // gives up the name's length, then its second sentence.
            let tx = x0 + 14.0 + em(28.0);
            let room = (w - tx - 12.0 - button_w("Delete") - 6.0 - button_w("Cancel") - 12.0).max(0.0);
            let q = delete_question(names, room);
            icon(ctx, x0 + 14.0, y + em(14.0), ph::WARNING, 20.0, t.danger_fg());
            text(ctx, tx, y + strip_h() / 2.0, &q, type_size::SM, Weight::Medium, t.text_primary());
            let bx = (tx + 12.0 + width(&q, type_size::SM, Weight::Medium)).min(w - 12.0 - button_w("Delete") - 6.0 - button_w("Cancel")).max(tx);
            let s = self.state.clone();
            let bw = button(ctx, bx, y + em(8.0), "Delete", Look::Danger, true, move || with(&s, Browser::delete_confirmed));
            let s = self.state.clone();
            button(ctx, bx + bw + 6.0, y + em(8.0), "Cancel", Look::Ghost, true, move || with(&s, |b| b.confirm_delete = None));
            y += strip_h();
        }

        // An older copy, open read-only (§8): it can be made current.
        if let Some(i) = b.at.filter(|_| b.place().is_some_and(|p| p.seal.older && !p.seal.locked)) {
            rect(ctx, x0, y, w - x0, strip_h(), t.accent_bg(), 0.0);
            icon(ctx, x0 + 14.0, y + em(14.0), ph::CLOCK, 20.0, t.text_secondary());
            let tx = x0 + 14.0 + em(28.0);
            let label = "Use this copy as current…";
            let room = (w - tx - 12.0 - button_w(label) - 12.0).max(0.0);
            let q = fit("This is an older copy of this stick, open read-only", type_size::SM, Weight::Medium, room);
            text(ctx, tx, y + strip_h() / 2.0, &q, type_size::SM, Weight::Medium, t.text_primary());
            let s = self.state.clone();
            button(ctx, w - 12.0 - button_w(label), y + em(8.0), label, Look::Secondary, b.stick_sheet.is_none(), move || with(&s, |b| b.ask_about(i)));
            y += strip_h();
        }

        // A Locked stick, or another persona's: what it is, not its files.
        if let Some((i, p)) = b.at.zip(b.place()).filter(|(_, p)| !p.browsable(&b.persona)) {
            self.sealed_pane(ctx, b, i, p, x0, w, y, h - status_h());
        } else {
            // Quick look takes the right of the list (opening-files.md §7).
            let lw = if b.look.is_some() { ((w - x0) * 0.45).clamp(em(260.0), em(480.0)).min((w - x0) * 0.6) } else { 0.0 };
            // Room for the Kind and Modified columns (em-sized) beside a useful name.
            self.list(ctx, b, x0, w - lw, y, h - status_h(), w - lw - x0 > em(620.0));
            if let Some(l) = &b.look {
                self.look_pane(ctx, l, w - lw, y, w, h - status_h());
            }
        }

        // The status line.
        let sy = h - status_h();
        rect(ctx, x0, sy, w - x0, status_h(), t.bg_surface(), 0.0);
        rect(ctx, x0, sy, w - x0, 1.0, t.border_default(), 0.0);
        let (msg, color) = match &b.notice {
            Some(n) => (n.text.clone(), if n.error { t.danger_fg() } else { t.text_secondary() }),
            None => {
                let items = b.entries.len();
                let mut s = format!("{items} item{}", if items == 1 { "" } else { "s" });
                if n > 0 {
                    s += &format!(", {n} selected");
                }
                (s, t.text_secondary())
            }
        };
        text(ctx, x0 + 16.0, sy + status_h() / 2.0, &fit(&msg, type_size::XS, Weight::Regular, w - x0 - 220.0), type_size::XS, Weight::Regular, color);
        let mut right = w - 16.0;
        if let Some(p) = b.place().filter(|p| p.stick && p.size > 0) {
            let r = format!("{} free of {}{}", fs::size_text(p.free), fs::size_text(p.size), if p.read_only { " · read-only" } else { "" });
            let rw = width(&r, type_size::XS, Weight::Regular);
            text(ctx, w - rw - 16.0, sy + status_h() / 2.0, &r, type_size::XS, Weight::Regular, t.text_tertiary());
            right -= rw + 16.0;
        }
        // A share's "connect at login" (D8), once dispensatord said which.
        if let Some((part, on)) = b.place().filter(|p| p.network).and_then(|p| b.at_login.get(&p.id).map(|on| (p.id.clone(), *on))) {
            let label = "Connect at login";
            let lw = width(label, type_size::XS, Weight::Regular) + 20.0;
            let x = right - lw;
            let s = self.state.clone();
            ctx.key(4100);
            area(ctx, Rect::new(x - 4.0, sy + 2.0, lw + 8.0, status_h() - 4.0), 4.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| b.want_at_login = Some((part.clone(), !on))));
            let cy = sy + status_h() / 2.0;
            rect(ctx, x, cy - 6.0, 12.0, 12.0, t.border_strong(), 3.0);
            rect(ctx, x + 1.0, cy - 5.0, 10.0, 10.0, if on { t.accent_fg() } else { t.bg_surface() }, 2.0);
            text(ctx, x + 18.0, cy, label, type_size::XS, Weight::Regular, t.text_secondary());
            ctx.pop();
        }
    }

    fn picker(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        let Mode::Picker { filter, app } = &b.mode else { return };
        // The heading: what is asked, and for whom.
        rect(ctx, 0.0, 0.0, w, pick_title_h(), t.bg_surface(), 0.0);
        rect(ctx, 0.0, pick_title_h() - 1.0, w, 1.0, t.border_default(), 0.0);
        let title = if !filter.title.is_empty() {
            filter.title.clone()
        } else {
            match (filter.kind, filter.multiple) {
                (Kind::Folder, _) => "Choose a folder".into(),
                (Kind::File | Kind::Workbook, true) => "Choose files".into(),
                (Kind::File | Kind::Workbook, false) => "Choose a file".into(),
                (Kind::Save, _) => format!("Save “{}”", filter.name),
            }
        };
        // Two lines in em, so they stay apart as the text grows.
        icon(ctx, 16.0, em(16.0), ph::FOLDER_OPEN, 20.0, t.accent_fg());
        let tx = 16.0 + em(30.0);
        text(ctx, tx, em(18.0), &fit(&title, type_size::MD, Weight::Semibold, w - tx - 24.0), type_size::MD, Weight::Semibold, t.text_primary());
        let who = match filter.kind {
            Kind::Save if filter.origin.is_empty() => format!("a new file from {app}"),
            Kind::Save => format!("a new file from {app} ({})", filter.origin),
            _ if filter.write => format!("for {app}, which may change it"),
            _ => format!("for {app}, to read"),
        };
        text(ctx, tx, em(37.0), &fit(&who, type_size::XS, Weight::Regular, w - tx - 24.0), type_size::XS, Weight::Regular, t.text_tertiary());

        let foot = h - pick_foot_h();
        let x0 = pick_side_for(w);
        self.sidebar(ctx, b, pick_title_h(), x0, foot);
        // The path row: up, then the path after the (em-sized) button.
        let py = pick_title_h();
        let s = self.state.clone();
        icon_button(ctx, x0 + 8.0, py + em(6.0), ph::ARROW_UP, !b.path.is_empty(), "Up", move || with(&s, Browser::up));
        self.crumbs(ctx, b, x0 + 18.0 + em(28.0), py + pick_path_h() / 2.0, w - 12.0);
        rect(ctx, x0, py + pick_path_h() - 1.0, w - x0, 1.0, t.border_default(), 0.0);
        self.list(ctx, b, x0, w, py + pick_path_h(), foot, false);

        // The answer.
        rect(ctx, 0.0, foot, w, pick_foot_h(), t.bg_surface(), 0.0);
        rect(ctx, 0.0, foot, w, 1.0, t.border_default(), 0.0);
        let pick = b.pick();
        let go = match filter.kind {
            Kind::File | Kind::Workbook => "Open",
            Kind::Folder => "Choose folder",
            Kind::Save => "Save",
        };
        // As wide as `button` draws them (their padding is em too).
        let (gw, cw) = (button_w(go), button_w("Cancel"));
        let by = foot + ((pick_foot_h() - em(32.0)) / 2.0).max(0.0);
        let s = self.state.clone();
        button(ctx, w - 16.0 - gw, by, go, Look::Primary, pick.is_ok() && b.outcome.is_none(), move || with(&s, Browser::choose));
        let s = self.state.clone();
        button(ctx, w - 24.0 - gw - cw, by, "Cancel", Look::Secondary, b.outcome.is_none(), move || with(&s, Browser::cancel));
        let (msg, color) = match (&b.notice, &pick, &b.outcome) {
            (_, _, Some(Outcome::Cancelled)) => ("Cancelled".to_string(), t.text_secondary()),
            (_, _, Some(_)) => ("Handing it over…".to_string(), t.text_secondary()),
            (Some(n), _, _) if n.error => (n.text.clone(), t.danger_fg()),
            (_, Ok(Outcome::Workbook { name, .. }), _) => (format!("“{name}”"), t.text_primary()),
            (_, Ok(Outcome::Files(f)), _) if f.len() == 1 => (format!("“{}”", f[0].0), t.text_primary()),
            (_, Ok(Outcome::Files(f)), _) => (format!("{} files", f.len()), t.text_primary()),
            (_, Ok(Outcome::Folder { name, .. }), _) => (format!("The folder “{name}”"), t.text_primary()),
            (_, Ok(Outcome::Save { dir, name, .. }), _) => {
                let into = dir.file_name().and_then(|n| n.to_str()).map_or_else(|| "here".to_string(), |d| format!("in “{d}”"));
                (format!("“{name}” {into}"), t.text_primary())
            }
            (_, Err(e), _) => (e.clone(), t.text_tertiary()),
            (_, Ok(Outcome::Cancelled | Outcome::WorkbookVersion { .. } | Outcome::RecentWorkbook { .. }), _) => (String::new(), t.text_tertiary()),
        };
        text(ctx, 16.0, foot + pick_foot_h() / 2.0, &fit(&msg, type_size::SM, Weight::Regular, w - 56.0 - gw - cw), type_size::SM, Weight::Regular, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::Place;
    use pergola::App;
    use std::sync::atomic::AtomicBool;

    fn place(d: &std::path::Path) -> Place {
        Place { id: "da0s1".into(), label: "16 GB stick".into(), root: d.to_path_buf(), stick: true, read_only: false, size: 16_000_000_000, free: 9_000_000_000, network: false, offline: false, seal: Default::default() }
    }

    fn texts(app: &App<ScriniumView>) -> Vec<String> {
        let tree = app.tree();
        let mut out = Vec::new();
        let mut stack: Vec<_> = tree.roots().collect();
        while let Some(id) = stack.pop() {
            if let Some(Node::Text { content, .. }) = tree.get(id) {
                out.push(content.clone());
            }
            stack.extend(tree.children_of(id).iter().copied());
        }
        out
    }

    fn app_for(b: Browser, size: (f32, f32)) -> (App<ScriniumView>, Mutable<Browser>) {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(b, dirty.clone());
        let v = ScriniumView::new(state.clone(), Mutable::with_dirty(TextEdit::new(""), dirty.clone()), Mutable::with_dirty(ScrollState::default(), dirty.clone()), Mutable::with_dirty(size, dirty.clone()));
        let mut app = App::new_with_flag(v, dirty);
        app.tick();
        (app, state)
    }

    #[test]
    fn the_manager_shows_places_path_rows_and_status() {
        let d = std::env::temp_dir().join(format!("scrinium-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Docs")).unwrap();
        std::fs::write(d.join("hello.txt"), "hello").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![place(&d)]);
        let (mut app, state) = app_for(b, (960.0, 600.0));
        let tx = texts(&app);
        for want in ["Places", "16 GB stick", "Docs", "hello.txt", "5 bytes", "2 items", "9.0 GB free of 16 GB", "New folder", "Paste"] {
            assert!(tx.iter().any(|t| t == want), "{want} in {tx:?}");
        }
        with(&state, |b| {
            b.press(1, Mods::default(), 0.0);
            b.ask_delete();
        });
        app.tick();
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t.starts_with("Delete “hello.txt”?")), "{tx:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A short window puts the actions on the path's row, so the list
    /// keeps its rows; a narrow one gives the places less of its width.
    #[test]
    fn a_short_window_shares_the_path_row_with_the_actions() {
        let d = std::env::temp_dir().join(format!("scrinium-view-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.txt"), "hello").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![place(&d)]);
        assert!(!compact(600.0) && compact(240.0));
        let (app, _) = app_for(b.clone(), (960.0, 240.0));
        let tree = app.tree();
        let at = |want: &str| tree.iter().find_map(|(_, n)| match n {
            Node::Text { content, rect, .. } if content == want => Some(*rect),
            _ => None,
        });
        let row = at("hello.txt").expect("a file row shows");
        assert!(row.y() + row.h() <= 240.0 - status_h(), "{row:?} above the status line");
        let new = at("Paste").expect("an action on the path's row");
        assert!(new.y() < toolbar_h(), "{new:?}");
        assert!(side_for(480.0) < side_w() && side_for(1600.0) == side_w());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// In a narrow window the delete question gives way and both its
    /// buttons stay inside the window.
    #[test]
    fn a_narrow_window_keeps_the_delete_buttons_inside() {
        let d = std::env::temp_dir().join(format!("scrinium-view-narrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a rather long name for a quarterly report draft.txt"), "x").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![place(&d)]);
        for w in [960.0, 640.0, 520.0] {
            let (mut app, state) = app_for(b.clone(), (w, 600.0));
            with(&state, |b| {
                b.press(0, Mods::default(), 0.0);
                b.ask_delete();
            });
            app.tick();
            let tree = app.tree();
            let ends: Vec<(String, f32)> = tree.iter().filter_map(|(_, n)| match n {
                Node::Text { content, rect, .. } if content == "Delete" || content == "Cancel" || content.starts_with("Delete “") => Some((content.clone(), rect.x() + rect.w())),
                _ => None,
            }).collect();
            assert!(ends.iter().any(|(c, _)| c == "Cancel"), "at {w}: {ends:?}");
            for (c, end) in &ends {
                assert!(*end <= w, "at {w}: {c:?} ends at {end}");
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_share_without_its_server_says_so() {
        let d = std::env::temp_dir().join(format!("scrinium-view-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut b = Browser::new(Mode::Manager);
        let share = Place { id: "share-63000".into(), label: "files on nas".into(), network: true, offline: true, ..place(&d) };
        b.set_places(vec![share]);
        b.at_login.insert("share-63000".into(), true);
        let (app, _) = app_for(b, (960.0, 600.0));
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "Reconnecting…"), "{tx:?}");
        assert!(tx.iter().any(|t| t == "Connect at login"), "{tx:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_locked_stick_shows_what_it_is_and_its_actions() {
        use crate::browser::{Seal, StickAsk};
        let d = std::env::temp_dir().join(format!("scrinium-view-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("secret.txt"), "x").unwrap();
        let mut b = Browser::new(Mode::Manager);
        let locked = Place { seal: Seal { locked: true, recovery: true, ..Default::default() }, ..place(&d) };
        let work = Place { id: "da1s1".into(), label: "8 GB stick".into(), seal: Seal { persona: "work".into(), ..Default::default() }, ..place(&d) };
        b.set_places(vec![locked, work]);
        let (mut app, state) = app_for(b, (960.0, 600.0));
        let tx = texts(&app);
        for want in ["Locked", "Encrypted stick from another machine or persona", "Enter recovery code…", "Erase and format…", "Work · 9.0 GB free"] {
            assert!(tx.iter().any(|t| t == want), "{want}: {tx:?}");
        }
        assert!(!tx.iter().any(|t| t == "secret.txt"), "nothing on it is listed");
        // Another persona's stick: shown, not opened.
        with(&state, |b| b.open_place(1));
        app.tick();
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "This is a Work stick"), "{tx:?}");
        assert!(!tx.iter().any(|t| t == "secret.txt"));
        // The recovery sheet holds Arca's field, not a text field of ours.
        with(&state, |b| {
            b.open_place(0);
            b.ask_about(0);
            b.ask_recovery();
        });
        app.tick();
        assert!(matches!(state.get_cloned().stick_sheet.map(|s| s.ask), Some(StickAsk::Recovery { keep: true })));
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "Type it into Arca's field: Scrinium never sees it."), "{tx:?}");
        // Under the sheet the pane keeps only its lock: its heading would show past the sheet.
        assert!(!tx.iter().any(|t| t.starts_with("Encrypted stick from another")), "{tx:?}");
        // Format: what will be erased, in this persona's words.
        with(&state, |b| {
            b.stick_sheet = None;
            b.persona = "work".into();
            b.ask_format(0);
        });
        app.tick();
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "Make “16 GB stick” a Work stick?"), "{tx:?}");
        assert!(tx.iter().any(|t| t == "Everything on it now will be erased."), "{tx:?}");
        // The code made with it: shown once, whole.
        with(&state, |b| {
            b.toggle_recovery();
            b.confirm_stick(None);
            b.media_done(Ok("da0s1 is now work's sealed stick; recovery code ABCDE-FGHJK-LMNPQ-RSTVW-XYZ23-45679".into()));
        });
        app.tick();
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "ABCDE-FGHJK-LMNPQ-RSTVW-XYZ23-45679"), "{tx:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_older_copy_offers_to_open_then_to_become_current() {
        use crate::browser::Seal;
        let d = std::env::temp_dir().join(format!("scrinium-view-older-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("old.txt"), "x").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![Place { seal: Seal { persona: "personal".into(), locked: true, older: true, ..Default::default() }, ..place(&d) }]);
        let (mut app, state) = app_for(b, (960.0, 600.0));
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "Open read-only…"), "{tx:?}");
        assert!(tx.iter().any(|t| t == "Older copy · Locked"), "{tx:?}");
        // Opened: read-only, listed, with the way to make it current.
        with(&state, |b| b.set_places(vec![Place { read_only: true, seal: Seal { persona: "personal".into(), older: true, ..Default::default() }, ..place(&d) }]));
        app.tick();
        let tx = texts(&app);
        assert!(tx.iter().any(|t| t == "old.txt"), "{tx:?}");
        assert!(tx.iter().any(|t| t == "Use this copy as current…"), "{tx:?}");
        assert!(tx.iter().any(|t| t == "Older copy"), "{tx:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Moving the cursor with keys scrolls its row whole into the list.
    #[test]
    fn the_cursor_row_is_scrolled_whole_into_view() {
        let d = std::env::temp_dir().join(format!("scrinium-view-reveal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut b = Browser::new(Mode::Picker { filter: limen::picker::Filter::file(), app: "org.example.notes".into() });
        b.set_places(vec![place(&d)]);
        let h = pick_title_h() + pick_path_h() + header_h() + 1.5 * row_h() + pick_foot_h();
        let lh = (1.5 * row_h()) as f64;
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(b, dirty.clone());
        let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let v = ScriniumView::new(state.clone(), Mutable::with_dirty(TextEdit::new(""), dirty.clone()), scroll.clone(), Mutable::with_dirty((640.0, h), dirty.clone()));
        let mut app = App::new_with_flag(v, dirty);
        app.tick();
        for _ in 0..6 {
            with(&state, |b| b.step(1, false));
            app.tick();
            let cur = state.get_cloned().cursor.unwrap() as f64;
            let y = scroll.get().physics.y;
            let (top, bottom) = (cur * row_h() as f64, (cur + 1.0) * row_h() as f64);
            assert!(top >= y - 0.5 && bottom <= y + lh + 0.5, "row {cur} ({top}..{bottom}) in view {y}..{}", y + lh);
        }
        assert!(scroll.get().physics.y > 0.0, "it scrolled");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_picker_says_what_and_for_whom() {
        let d = std::env::temp_dir().join(format!("scrinium-view-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.txt"), "hello").unwrap();
        let f = limen::picker::Filter { title: "Open a note".into(), ..limen::picker::Filter::file() };
        let mut b = Browser::new(Mode::Picker { filter: f, app: "org.example.notes".into() });
        b.set_places(vec![place(&d)]);
        let (mut app, state) = app_for(b, (640.0, 420.0));
        let tx = texts(&app);
        for want in ["Open a note", "for org.example.notes, to read", "Open", "Cancel", "Select a file"] {
            assert!(tx.iter().any(|t| t == want), "{want} in {tx:?}");
        }
        assert!(!tx.iter().any(|t| t == "New folder"), "a picker changes nothing");
        // It states a minimum for frescod to grow its sheet to, inside
        // the catalogue's 640 x 440 at text size 100.
        let m = app.tree().min_size().expect("the picker states its minimum");
        assert!(m.w <= 640.0 && m.h <= 440.0 && m.w >= 500.0, "{m:?}");
        with(&state, |b| b.press(0, Mods::default(), 0.0));
        app.tick();
        assert!(texts(&app).iter().any(|t| t == "“hello.txt”"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keys() {
        let d = std::env::temp_dir().join(format!("scrinium-view-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("A")).unwrap();
        std::fs::write(d.join("b.txt"), "b").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![place(&d)]);
        let state = Mutable::new(b);
        let field = Mutable::new(TextEdit::new(""));
        let k = |key: Key, chars: &str, ctrl: bool, shift: bool| Event::Key {
            kind: KeyEventKind::Down,
            key,
            modifiers: pergola::Modifiers { ctrl, shift, ..Default::default() },
            chars: chars.into(),
        };
        assert!(on_key(&state, &field, &k(Key::Down, "", false, false)));
        assert_eq!(state.get_cloned().selected, [0]);
        assert!(on_key(&state, &field, &k(Key::Enter, "", false, false)));
        assert_eq!(state.get_cloned().path, ["A"]);
        assert!(on_key(&state, &field, &k(Key::Backspace, "", false, false)));
        assert!(state.get_cloned().path.is_empty());
        assert!(on_key(&state, &field, &k(Key::Char, "N", true, true)));
        assert_eq!(state.get_cloned().edit, Some(Edit::NewFolder));
        assert_eq!(field.get_cloned().text(), "New folder");
        assert!(!on_key(&state, &field, &k(Key::Char, "a", false, false)), "typing goes to the field");
        assert!(on_key(&state, &field, &k(Key::Escape, "", false, false)));
        assert_eq!(state.get_cloned().edit, None);
        let _ = std::fs::remove_dir_all(&d);
    }
}

