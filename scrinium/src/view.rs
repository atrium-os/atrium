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

use crate::browser::{Browser, Edit, Mode, Mods, Outcome};
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
}

// Layout sizes at 100% text; each grows with the system text size
// (resolution-independence.md §6.2), so rows and bars fit their labels.
fn em(pt: f32) -> f32 { pergola::theme::size::em(pt) }
pub fn side_w() -> f32 { em(208.0) }
pub fn pick_side_w() -> f32 { em(176.0) }
pub fn toolbar_h() -> f32 { em(48.0) }
pub fn actions_h() -> f32 { em(40.0) }
pub fn strip_h() -> f32 { em(48.0) }
pub fn header_h() -> f32 { em(28.0) }
pub fn status_h() -> f32 { em(28.0) }
pub fn row_h() -> f32 { em(30.0) }
pub fn place_h() -> f32 { em(44.0) }
pub fn pick_title_h() -> f32 { em(52.0) }
pub fn pick_foot_h() -> f32 { em(60.0) }

pub struct ScriniumView {
    pub state: Mutable<Browser>,
    /// The name being typed (new folder, rename).
    pub field: Mutable<TextEdit>,
    pub scroll: Mutable<ScrollState>,
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
    /// The actions row's "More" menu, where actions go that no longer fit.
    pub more: Mutable<pergola::widgets::MenuState>,
}

impl ScriniumView {
    pub fn new(state: Mutable<Browser>, field: Mutable<TextEdit>, scroll: Mutable<ScrollState>, size: Mutable<(f32, f32)>) -> Self {
        let cred_state = Mutable::new(CredentialState::default());
        let cred = CredentialField::password(cred_state.clone());
        Self { state, field, scroll, size, mods: Arc::new(Mutex::new(Mods::default())), clock: Instant::now(), cred, cred_state, more: Mutable::new(Default::default()) }
    }
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

/// `s`, cut with an ellipsis to fit `max` px.
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

/// A 28 px ghost icon button.
fn icon_button(ctx: &mut Ctx, x: f32, y: f32, g: char, enabled: bool, tip: &str, on: impl Fn() + 'static) {
    let t = ctx.theme;
    let r = Rect::new(x, y, 28.0, 28.0);
    let id = if enabled { area(ctx, r, 6.0, Color::TRANSPARENT, t.state_hover(), on) } else { ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 6.0 }) };
    icon(ctx, x + 4.0, y + 4.0, g, 20.0, if enabled { t.text_secondary() } else { t.text_disabled() });
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
fn button(ctx: &mut Ctx, x: f32, y: f32, label: &str, look: Look, enabled: bool, on: impl Fn() + 'static) -> f32 {
    let t = ctx.theme;
    let (w, h) = (width(label, type_size::SM, Weight::Medium) + em(28.0), em(32.0));
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
    match fs::kind_of(name, dir) {
        "Folder" => ph::FOLDER,
        "Image" => ph::IMAGE,
        "PDF" => ph::FILE_PDF,
        "Audio" => ph::MUSIC_NOTE_SIMPLE,
        "Video" => ph::FILM_STRIP,
        "Archive" => ph::PACKAGE,
        _ => ph::FILE_TEXT,
    }
}

impl View for ScriniumView {
    fn render(&self, ctx: &mut Ctx) {
        let b = self.state.get_cloned();
        let (w, h) = self.size.get();
        let t = ctx.theme;
        rect(ctx, 0.0, 0.0, w, h, t.bg_canvas(), 0.0);
        match &b.mode {
            Mode::Manager => {
                // Laid out by hand, so the minimum is stated: the places,
                // a name and a size column, a few rows (em: it grows with
                // the text size).
                ctx.min_size(pergola::geom::Size::new(side_w() + em(360.0), em(360.0)));
                self.manager(ctx, &b, w, h);
                if b.connect.is_some() {
                    self.connect_sheet(ctx, &b, w, h);
                }
                if b.key_changed.is_some() {
                    self.key_changed_sheet(ctx, &b, w, h);
                }
            }
            Mode::Picker { .. } => self.picker(ctx, &b, w, h),
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
        text(ctx, 16.0, top + 22.0, "Places", type_size::XS, Weight::Semibold, t.text_tertiary());
        let manager = b.picker().is_none();
        if manager {
            self.servers(ctx, b, top, sw, h);
        }
        if b.places.is_empty() {
            text(ctx, 16.0, top + 54.0, "Plug in a USB stick", type_size::SM, Weight::Regular, t.text_tertiary());
            return;
        }
        for (i, p) in b.places.iter().enumerate() {
            let y = top + 38.0 + i as f32 * (place_h() + 2.0);
            let on = b.at == Some(i);
            let s = self.state.clone();
            ctx.key(1000 + i as u64);
            area(ctx, Rect::new(8.0, y, sw - 16.0, place_h()), 6.0, if on { t.selection_idle() } else { Color::TRANSPARENT }, t.state_hover(), move || with(&s, |b| b.open_place(i)));
            let g = if p.network { ph::GLOBE } else if p.stick { ph::USB } else { ph::FOLDER };
            icon(ctx, 16.0, y + 7.0, g, 20.0, if on { t.accent_fg() } else { t.text_secondary() });
            let room = sw - 16.0 - 44.0 - if p.stick && manager { 30.0 } else { 0.0 };
            text(ctx, 44.0, y + 15.0, &fit(&p.label, type_size::SM, Weight::Medium, room), type_size::SM, Weight::Medium, t.text_primary());
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
                let free = if p.size > 0 { format!("{} free", fs::size_text(p.free)) } else { "USB stick".into() };
                if p.read_only { format!("{free} · read-only") } else { free }
            } else {
                p.root.to_string_lossy().into_owned()
            };
            text(ctx, 44.0, y + 31.0, &fit(&sub, type_size::XS, Weight::Regular, room), type_size::XS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            if p.stick && manager {
                let s = self.state.clone();
                icon_button(ctx, sw - 44.0, y + 8.0, ph::EJECT, true, if p.network { "Disconnect" } else { "Eject" }, move || with(&s, |b| b.eject(i)));
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
        let total = |from: usize| names[from..].iter().map(|n| width(n, type_size::SM, Weight::Medium) + 28.0).sum::<f32>();
        while first + 1 < names.len() && x + total(first) > max_x {
            first += 1;
        }
        let mut cx = x;
        if first > 0 {
            text(ctx, cx, cy, "…", type_size::SM, Weight::Regular, t.text_tertiary());
            cx += 14.0;
            icon(ctx, cx, cy - 7.0, ph::CARET_RIGHT, 14.0, t.text_tertiary());
            cx += 18.0;
        }
        for (d, n) in names.iter().enumerate().skip(first) {
            let last = d + 1 == names.len();
            let label = fit(n, type_size::SM, Weight::Medium, (max_x - cx - 10.0).max(24.0));
            let wd = width(&label, type_size::SM, Weight::Medium) + 10.0;
            let s = self.state.clone();
            ctx.key(2000 + d as u64);
            area(ctx, Rect::new(cx - 5.0, cy - 13.0, wd, 26.0), 5.0, Color::TRANSPARENT, t.state_hover(), move || with(&s, |b| b.up_to(d)));
            text(ctx, cx, cy, &label, type_size::SM, if last { Weight::Semibold } else { Weight::Medium }, if last { t.text_primary() } else { t.text_secondary() });
            ctx.pop();
            cx += wd;
            if !last {
                icon(ctx, cx, cy - 7.0, ph::CARET_RIGHT, 14.0, t.text_tertiary());
                cx += 18.0;
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
        head(ctx, 44.0, "Name");
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
                (None, Some(_)) => "This folder is empty".into(),
            };
            let tw = width(&msg, type_size::SM, Weight::Regular);
            text(ctx, x0 + (lw - tw) / 2.0, top + lh.min(200.0) / 2.0, &msg, type_size::SM, Weight::Regular, t.text_tertiary());
            return;
        }
        let entries = b.entries.clone();
        let selected = b.selected.clone();
        let handlers: Vec<_> = (0..entries.len()).map(|i| std::rc::Rc::new(self.press_handler(i))).collect();
        let picking_files = b.picker().map(|f| f.kind == Kind::File);
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
            icon(ctx, 16.0, 7.0, file_icon(&e.name, e.dir, e.link), 16.0, if e.dir { t.accent_fg() } else { t.text_secondary() });
            let name_w = if kind_w > 0.0 { kind_x - 52.0 } else { size_x - 52.0 };
            text(ctx, 44.0, row_h() / 2.0, &fit(&e.name, type_size::SM, Weight::Regular, name_w), type_size::SM, Weight::Regular, if dim { t.text_secondary() } else { fg });
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

    fn manager(&self, ctx: &mut Ctx, b: &Browser, w: f32, h: f32) {
        let t = ctx.theme;
        self.sidebar(ctx, b, 0.0, side_w(), h);
        let x0 = side_w();
        // Toolbar: back, up, the path.
        rect(ctx, x0, toolbar_h() - 1.0, w - x0, 1.0, t.border_default(), 0.0);
        let s = self.state.clone();
        icon_button(ctx, x0 + 10.0, 10.0, ph::ARROW_LEFT, b.can_go_back(), "Back (Alt-Left)", move || with(&s, Browser::go_back));
        let s = self.state.clone();
        icon_button(ctx, x0 + 42.0, 10.0, ph::ARROW_UP, !b.path.is_empty(), "Up (Backspace)", move || with(&s, Browser::up));
        self.crumbs(ctx, b, x0 + 84.0, toolbar_h() / 2.0, w - 16.0);

        // What can be done here.
        let ay = toolbar_h() + 4.0;
        let open = b.place().is_some() && b.unreadable.is_none();
        let writable = open && !b.place().is_some_and(|p| p.read_only);
        let n = b.selected.len();
        // A toolbar: when the text grows or the window narrows, what no
        // longer fits goes into its "More" menu (resolution-independence.md §6.3).
        let hid = if b.show_hidden { "Hide hidden files" } else { "Show hidden files" };
        let hw = width(hid, type_size::SM, Weight::Medium) + em(28.0);
        let (s1, f1, s2, f2) = (self.state.clone(), self.field.clone(), self.state.clone(), self.field.clone());
        let (s3, s4, s5, s6) = (self.state.clone(), self.state.clone(), self.state.clone(), self.state.clone());
        let items = vec![
            ToolItem::new("New folder", move || begin_edit(&s1, &f1, false)).priority(2).enabled(writable),
            ToolItem::new("Rename", move || begin_edit(&s2, &f2, true)).priority(1).enabled(writable && n == 1),
            ToolItem::new("Delete", move || with(&s3, Browser::ask_delete)).priority(1).enabled(writable && n > 0),
            ToolItem::new("Copy", move || with(&s4, |b| b.copy(false))).priority(3).enabled(open && n > 0),
            ToolItem::new("Cut", move || with(&s5, |b| b.copy(true))).priority(2).enabled(writable && n > 0),
            ToolItem::new("Paste", move || with(&s6, Browser::paste)).priority(3).enabled(writable && b.clip.is_some()),
        ];
        let tx = x0 + 8.0;
        Toolbar::new(Rect::new(tx, ay, (w - hw - 16.0 - tx).max(0.0), actions_h() - 8.0), self.more.clone(), items).render(ctx);
        let s = self.state.clone();
        button(ctx, w - hw - 8.0, ay + ((actions_h() - 8.0 - em(32.0)) / 2.0).max(0.0), hid, Look::Ghost, open, move || with(&s, Browser::toggle_hidden));
        let mut y = toolbar_h() + actions_h();
        rect(ctx, x0, y - 1.0, w - x0, 1.0, t.border_default(), 0.0);

        // A name being typed, or a question.
        if let Some(edit) = &b.edit {
            rect(ctx, x0, y, w - x0, strip_h(), t.accent_bg(), 0.0);
            let label = match edit {
                Edit::NewFolder => "New folder:".to_string(),
                Edit::Rename(n) => format!("Rename “{}” to:", fit(n, type_size::SM, Weight::Medium, 160.0)),
            };
            text(ctx, x0 + 16.0, y + strip_h() / 2.0, &label, type_size::SM, Weight::Medium, t.text_primary());
            let fx = x0 + 24.0 + width(&label, type_size::SM, Weight::Medium);
            let fw = (w - fx - 200.0).clamp(120.0, 320.0);
            let s = self.state.clone();
            let holder = ctx.push(Node::Rect { rect: Rect::new(fx, y + 8.0, fw, 32.0), fill: Color::TRANSPARENT, radius: 0.0 });
            TextField::new(self.field.clone()).at(fx, y + 8.0).width(fw).on_submit(move |v| with(&s, |b| b.commit_edit(v))).render(ctx);
            ctx.pop();
            if let Some(&bg) = ctx.tree.children_of(holder).get(2) {
                ctx.autofocus(bg);
            }
            let (s, f) = (self.state.clone(), self.field.clone());
            let go = if matches!(edit, Edit::NewFolder) { "Create" } else { "Rename" };
            let bx = fx + fw + 10.0;
            let bw = button(ctx, bx, y + 8.0, go, Look::Primary, true, move || {
                let v = f.get_cloned().text().to_string();
                with(&s, |b| b.commit_edit(&v));
            });
            let s = self.state.clone();
            button(ctx, bx + bw + 6.0, y + 8.0, "Cancel", Look::Ghost, true, move || with(&s, Browser::cancel_edit));
            y += strip_h();
        } else if let Some(names) = &b.confirm_delete {
            rect(ctx, x0, y, w - x0, strip_h(), t.accent_bg(), 0.0);
            let q = if names.len() == 1 {
                format!("Delete “{}”? It can't be brought back.", fit(&names[0], type_size::SM, Weight::Medium, 220.0))
            } else {
                format!("Delete {} items? They can't be brought back.", names.len())
            };
            icon(ctx, x0 + 14.0, y + 14.0, ph::WARNING, 20.0, t.danger_fg());
            text(ctx, x0 + 42.0, y + strip_h() / 2.0, &q, type_size::SM, Weight::Medium, t.text_primary());
            let bx = x0 + 54.0 + width(&q, type_size::SM, Weight::Medium);
            let s = self.state.clone();
            let bw = button(ctx, bx, y + 8.0, "Delete", Look::Danger, true, move || with(&s, Browser::delete_confirmed));
            let s = self.state.clone();
            button(ctx, bx + bw + 6.0, y + 8.0, "Cancel", Look::Ghost, true, move || with(&s, |b| b.confirm_delete = None));
            y += strip_h();
        }

        self.list(ctx, b, x0, w, y, h - status_h(), w - x0 > 620.0);

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
                (Kind::File, true) => "Choose files".into(),
                (Kind::File, false) => "Choose a file".into(),
            }
        };
        icon(ctx, 16.0, 16.0, ph::FOLDER_OPEN, 20.0, t.accent_fg());
        text(ctx, 46.0, 18.0, &fit(&title, type_size::MD, Weight::Semibold, w - 70.0), type_size::MD, Weight::Semibold, t.text_primary());
        let who = if filter.write { format!("for {app}, which may change it") } else { format!("for {app}, to read") };
        text(ctx, 46.0, 37.0, &fit(&who, type_size::XS, Weight::Regular, w - 70.0), type_size::XS, Weight::Regular, t.text_tertiary());

        let foot = h - pick_foot_h();
        self.sidebar(ctx, b, pick_title_h(), pick_side_w(), foot);
        let x0 = pick_side_w();
        // The path row.
        let py = pick_title_h();
        let s = self.state.clone();
        icon_button(ctx, x0 + 8.0, py + 6.0, ph::ARROW_UP, !b.path.is_empty(), "Up", move || with(&s, Browser::up));
        self.crumbs(ctx, b, x0 + 46.0, py + 20.0, w - 12.0);
        rect(ctx, x0, py + 39.0, w - x0, 1.0, t.border_default(), 0.0);
        self.list(ctx, b, x0, w, py + 40.0, foot, false);

        // The answer.
        rect(ctx, 0.0, foot, w, pick_foot_h(), t.bg_surface(), 0.0);
        rect(ctx, 0.0, foot, w, 1.0, t.border_default(), 0.0);
        let pick = b.pick();
        let go = match filter.kind {
            Kind::File => "Open",
            Kind::Folder => "Choose folder",
        };
        let gw = width(go, type_size::SM, Weight::Medium) + 28.0;
        let cw = width("Cancel", type_size::SM, Weight::Medium) + 28.0;
        let s = self.state.clone();
        button(ctx, w - 16.0 - gw, foot + 14.0, go, Look::Primary, pick.is_ok() && b.outcome.is_none(), move || with(&s, Browser::choose));
        let s = self.state.clone();
        button(ctx, w - 24.0 - gw - cw, foot + 14.0, "Cancel", Look::Secondary, b.outcome.is_none(), move || with(&s, Browser::cancel));
        let (msg, color) = match (&b.notice, &pick, &b.outcome) {
            (_, _, Some(Outcome::Cancelled)) => ("Cancelled".to_string(), t.text_secondary()),
            (_, _, Some(_)) => ("Handing it over…".to_string(), t.text_secondary()),
            (Some(n), _, _) if n.error => (n.text.clone(), t.danger_fg()),
            (_, Ok(Outcome::Files(f)), _) if f.len() == 1 => (format!("“{}”", f[0].0), t.text_primary()),
            (_, Ok(Outcome::Files(f)), _) => (format!("{} files", f.len()), t.text_primary()),
            (_, Ok(Outcome::Folder { name, .. }), _) => (format!("The folder “{name}”"), t.text_primary()),
            (_, Err(e), _) => (e.clone(), t.text_tertiary()),
            (_, Ok(Outcome::Cancelled), _) => (String::new(), t.text_tertiary()),
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
        Place { id: "da0s1".into(), label: "16 GB stick".into(), root: d.to_path_buf(), stick: true, read_only: false, size: 16_000_000_000, free: 9_000_000_000, network: false, offline: false }
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
