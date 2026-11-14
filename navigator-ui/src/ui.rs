//! Drawing the window (docs/spec/navigator-ui.md §5.2): the rail, the
//! toolbar with the Where field and the tier chip, the prompt row, the
//! page slot (a placeholder until step 2), Start, the stop page, the
//! status bar, and the suggestion list and tier popover drawn in the
//! window where the real ones become Fresco pop-ups (fresco-popups.md).
//!
//! Dimensions and colours follow the visual language (rail rows 32 px,
//! path rows 26, toolbar 40, Phosphor icons 16 and 20, one accent).
//! Drawing order matters twice over: frescod draws by kind (rects never
//! cover glyphs), and nodes that come and go (the field, the list) are
//! drawn last so the ids before them stay put across a press.

use std::time::Instant;

use pergola::geom::{Axis, Rect};
use pergola::layout::{Align, FlexStyle};
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Glyph;
use pergola::{Color, Mutable, NodeId, TextStyle};

use crate::fake;
use crate::shell::em;
use crate::history::Clear;
use crate::shell::{self, Geo, Shell, DROP_LABEL, DROP_ROW, HIST_DAY, PAGE_SLOT};
use crate::trail::{self, Light, Prompt, RowMedia, Tier};
use crate::where_field::Source;

/// Phosphor codepoints (fonts/phosphor/Phosphor.ttf, regular weight).
pub mod ph {
    pub use pergola::widgets::phosphor::*;
    pub const ARROW_LEFT: char = '\u{e058}';
    pub const ARROW_RIGHT: char = '\u{e06c}';
    pub const ARROW_CLOCKWISE: char = '\u{e036}';
    pub const TEXT_AA: char = '\u{e6ee}';
    pub const DOTS_THREE: char = '\u{e1fe}';
    pub const STAR: char = '\u{e46a}';
    pub const USER: char = '\u{e4c2}';
    pub const USER_CIRCLE: char = '\u{e4c4}';
    pub const CARET_DOWN: char = '\u{e136}';
    pub const CARET_UP: char = '\u{e13c}';
    pub const MINUS: char = '\u{e32a}';
    pub const DOWNLOAD: char = '\u{e20c}';
    pub const CLOCK: char = '\u{e1a0}';
    pub const BOOKMARK: char = '\u{e0ea}';
    pub const EYE_SLASH: char = '\u{e224}';
    pub const FILE_TEXT: char = '\u{e23a}';
    pub const FILM_STRIP: char = '\u{e792}';
    pub const LIGHTNING: char = '\u{e2de}';
    pub const SHIELD_CHECK: char = '\u{e40c}';
    pub const WARNING: char = '\u{e4e0}';
    pub const SIGN_OUT: char = '\u{e42a}';
    pub const KEY: char = '\u{e2d6}';
    pub const PASSWORD: char = '\u{e752}';
    pub const TRASH_SIMPLE: char = '\u{e4a8}';
}

/// The app clock for the prompt row's 500 ms rule.
pub fn now_ms() -> f64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e3
}

pub struct ShellView {
    pub state: Mutable<Shell>,
}

/// Change the shell from a handler.
fn with(state: &Mutable<Shell>, f: impl FnOnce(&mut Shell)) {
    let mut s = state.get_cloned();
    f(&mut s);
    trace(&mut s);
    state.set(s);
}

/// Log a press's note, and with `NAVIGATOR_UI_TRACE` set, where the
/// window is after it (the VM check reads this).
pub fn trace(s: &mut Shell) {
    if let Some(n) = s.last_note.take() {
        eprintln!("navigator-ui: {n}");
    }
    if std::env::var_os("NAVIGATOR_UI_AX").is_some() {
        // The accessibility tree (§10), when it changed.
        static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
        let o = crate::ax::outline(&crate::ax::tree(s));
        let mut last = LAST.lock().unwrap();
        if *last != o {
            for l in o.lines() {
                eprintln!("navigator-ui: ax {l}");
            }
            *last = o;
        }
    }
    if std::env::var_os("NAVIGATOR_UI_TRACE").is_some() {
        let t = s.store.trail();
        eprintln!(
            "navigator-ui: trace trail=\"{}\" pages={} trails={} earlier={} where={} prompt={} rail={} url={} live={} page={},{} w={}",
            t.title(),
            t.pages.len(),
            s.store.trails.len(),
            s.store.earlier.len(),
            s.editing.as_ref().map_or("-".to_string(), |e| format!("\"{}\"", e.edit.text())),
            t.page().and_then(|p| p.asked.as_ref()).map_or("-", |a| match a.prompt {
                Prompt::Download { .. } => "download",
                Prompt::NotPrivate { .. } => "not-private",
                Prompt::FetchAgain => "fetch-again",
                Prompt::Play { .. } => "play",
                Prompt::Handoff { .. } => "handoff",
                Prompt::OpenApp { .. } => "app",
                Prompt::Stored { .. } => "stored",
            }),
            s.rail_w(),
            t.page().map_or("-", |p| p.url.as_str()),
            match &s.live {
                None => "-".to_string(),
                Some(crate::shell::LivePage::Loading(_)) => "loading".into(),
                Some(crate::shell::LivePage::Shown(_)) => "shown".into(),
                Some(crate::shell::LivePage::Failed { why, .. }) => format!("failed:{}", why.replace(' ', "_")),
                Some(crate::shell::LivePage::File { name, .. }) => format!("file:{}", name.replace(' ', "_")),
            },
            s.geo().page.x(),
            s.geo().page.y(),
            s.w,
        );
    }
}

// -- drawing helpers ---------------------------------------------------------

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose cap band is centred on `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> NodeId {
    // The line as it is drawn: at the text size, centred on `cy`.
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, st: &TextStyle) -> f32 {
    Node::measure_text(s, st).w
}

/// `s`, cut with an ellipsis to fit `max` px.
fn fit(s: &str, st: &TextStyle, max: f32) -> String {
    if max <= 0.0 {
        return String::new();
    }
    if width(s, st) <= max {
        return s.into();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t, st) <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}

fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) -> NodeId {
    ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius })
}

/// A 1 px edge round a box.
fn boxed(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, edge: Color, fill: Color, radius: f32) -> NodeId {
    let id = rect(ctx, x, y, w, h, edge, radius);
    ctx.tree.insert(Some(id), Node::Rect { rect: Rect::new(x + 1.0, y + 1.0, w - 2.0, h - 2.0), fill, radius: (radius - 1.0).max(0.0) });
    id
}

fn icon(ctx: &mut Ctx, x: f32, y: f32, g: char, size: f32, color: Color) -> NodeId {
    Glyph::new(g).size(size).color(color).at(x, y).emit(ctx)
}

/// A pressable area: pushed as the parent of what is drawn in it (so a
/// press on its text or icon reaches it), filled `hover` while hovered.
/// The caller pops it.
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

/// A 28 pt ghost icon button at the text size (visual language §14:
/// overlay on hover; resolution-independence.md §6.2).
fn icon_button(ctx: &mut Ctx, x: f32, y: f32, g: char, enabled: bool, color: Color, tip: &str, on: impl Fn() + 'static) {
    let t = ctx.theme;
    let r = Rect::new(x, y, em(28.0), em(28.0));
    let id = if enabled {
        area(ctx, r, em(6.0), Color::TRANSPARENT, t.state_hover(), on)
    } else {
        ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: em(6.0) })
    };
    icon(ctx, x + em(4.0), y + em(4.0), g, 20.0, if enabled { color } else { t.text_disabled() });
    ctx.pop();
    ctx.tooltip(id, tip.to_string());
}

/// Texts in one row on a shared baseline, laid out by Pergola.
fn inline(ctx: &mut Ctx, x: f32, cy: f32, gap: f32, runs: &[(String, TextStyle)]) {
    let y = cy - 0.5 * em(runs.first().map(|r| r.1.size).unwrap_or(13.0));
    let r = ctx.push(Node::stack(Axis::Horizontal, Rect::new(x, y, 0.0, 0.0), gap));
    ctx.set_flex(r, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
    for (s, st) in runs {
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: s.clone(), style: st.clone() });
    }
    ctx.pop();
}

/// The registrable domain's monogram: a neutral tile and one letter. No
/// favicons (§1, D6).
fn monogram(ctx: &mut Ctx, x: f32, y: f32, url: &str, strong: bool) {
    let t = ctx.theme;
    let site = trail::site(url);
    let letter: String = site.chars().next().map_or("·".into(), |c| c.to_uppercase().collect());
    rect(ctx, x, y, em(20.0), em(20.0), if strong { t.text_primary() } else { t.border_strong() }, em(4.0));
    let st = sans(type_size::XS, Weight::Semibold, if strong { t.bg_canvas() } else { t.text_primary() });
    let lw = width(&letter, &st);
    text(ctx, x + (em(20.0) - lw) / 2.0, y + em(10.0), &letter, type_size::XS, Weight::Semibold, if strong { t.bg_canvas() } else { t.text_primary() });
}

fn light_glyph(l: Light) -> char {
    match l {
        Light::Green => ph::SHIELD_CHECK,
        Light::Amber => ph::WARNING,
        Light::Red => ph::PROHIBIT,
    }
}

fn light_color(ctx: &Ctx, l: Light) -> Color {
    let t = ctx.theme;
    match l {
        Light::Green => t.success_fg(),
        Light::Amber => t.warning_fg(),
        Light::Red => t.danger_fg(),
    }
}

fn media_glyph(m: RowMedia) -> char {
    match m {
        RowMedia::Sound => ph::SPEAKER_SIMPLE_HIGH,
        RowMedia::Muted => ph::SPEAKER_SIMPLE_X,
        RowMedia::Silent => ph::PLAY,
    }
}

/// The tier sentence (§6.3), as the popover and the chip's tooltip say it.
pub fn tier_sentence(tier: Tier, site: &str) -> String {
    match tier {
        Tier::Static => format!("This page is text and pictures. No code from {site} ran to make it, and none runs now."),
        Tier::Replayed => format!("{site}'s scripts ran once, sealed off, to record what this page can do. What you click now replays those recordings. None of the site's code runs now."),
        Tier::Live => format!("{site}'s JavaScript is running now, in a jail of its own. It can't see your other pages, your files or other sites' logins. It can see what you type into this page and send it to its own site."),
    }
}

// -- the view -----------------------------------------------------------------

impl View for ShellView {
    fn render(&self, ctx: &mut Ctx) {
        let s = self.state.get_cloned();
        let g = s.geo();
        let t = ctx.theme;
        rect(ctx, 0.0, 0.0, g.w, g.h, t.bg_canvas(), 0.0);

        // The page area first, then what frames it.
        let page = s.store.trail().page().cloned();
        let mut hovered: Option<String> = None;
        match &page {
            _ if s.history.is_some() => history(ctx, &s, &g, &self.state),
            None => start(ctx, &s, &g, &self.state),
            Some(p) if p.facts.light == Light::Red => stop_page(ctx, &g, p, &self.state),
            Some(p) => hovered = page_slot(ctx, &g, p, &self.state),
        }
        if let Some(r) = g.status {
            status_bar(ctx, &s, r, hovered.or_else(|| s.hover_hint.clone()));
        }
        if let (Some(r), Some(p)) = (g.prompt, &page) {
            if let Some(a) = &p.asked {
                prompt_row(ctx, r, &a.prompt, &self.state);
            }
        }
        if let (Some(r), Some(f)) = (g.find, &s.find) {
            find_bar(ctx, r, f, &self.state);
        }
        if g.rail > em(shell::RAIL_NARROW) {
            rail(ctx, &s, &g, &self.state);
        } else {
            rail_narrow(ctx, &s, &g, &self.state);
        }
        toolbar(ctx, &s, &g, &self.state);
        if let Some(r) = g.drop {
            suggestions(ctx, &s, r, &self.state);
        }
        // The tier popover and Aa are pop-up windows where there is a
        // frescod (the page's window is above this one), else drawn here.
        if let (Some(r), Some(p)) = (g.popover, page.clone()) {
            let st = self.state.clone();
            floating(ctx, r, shell::chip_rect(&g), move || with(&st, |s| s.popover = false), move |ctx| popover(ctx, r, &p));
        }
        if let (Some(r), Some(p)) = (g.presence_menu, &page) {
            let (st, st2) = (self.state.clone(), self.state.clone());
            let site = trail::site(&p.url).to_string();
            let knows = s.store.knows(&site);
            let accounts = s.accounts.as_ref().filter(|(a, _)| *a == site).map(|(_, a)| a.clone());
            floating(ctx, r, g.presence, move || with(&st, |s| s.presence_open = false), move |ctx| presence_menu(ctx, r, &site, knows, accounts, &st2));
        }
        if let (Some(r), Some(p)) = (g.reading, &page) {
            let (st, st2) = (self.state.clone(), self.state.clone());
            let (prefs, site) = (s.store.reading(), trail::site(&p.url).to_string());
            floating(ctx, r, g.aa, move || with(&st, |s| s.reading_open = false), move |ctx| reading_controls(ctx, r, prefs, &site, &st2));
        }
    }
}

/// A pop-up window hung below `anchor` (fresco-popups.md), or, with no
/// frescod to place one (screenshots, tests), drawn at `r` in the window.
fn floating(ctx: &mut Ctx, r: Rect, anchor: Rect, on_dismiss: impl Fn() + 'static, draw: impl FnOnce(&mut Ctx) + 'static) {
    if !ctx.popups_enabled() {
        return draw(ctx);
    }
    ctx.overlay(move |ctx| {
        let spec = pergola::popup::Spec { anchor, side: pergola::popup::Side::Below, align: pergola::popup::Align::Start, grab: true };
        ctx.popup(r, spec, on_dismiss, draw);
    });
}

// -- the rail -----------------------------------------------------------------

fn rail(ctx: &mut Ctx, s: &Shell, g: &Geo, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let w = g.rail;
    rect(ctx, 0.0, 0.0, w, g.h, t.bg_surface(), 0.0);
    rect(ctx, w, 0.0, 1.0, g.h, t.border_default(), 0.0);

    // Persona (for new trails) and New trail.
    {
        let st2 = st.clone();
        rect(ctx, em(12.0), em(6.0), em(122.0), em(28.0), t.border_control(), em(6.0));
        area(ctx, Rect::new(em(13.0), em(7.0), em(120.0), em(26.0)), em(5.0), t.bg_elevated(), t.state_hover(), move || with(&st2, |s| {
            s.store.persona = match s.store.persona {
                trail::Persona::Personal => trail::Persona::Unrecorded,
                trail::Persona::Unrecorded => trail::Persona::Personal,
            };
        }));
        let unrec = s.store.persona == trail::Persona::Unrecorded;
        icon(ctx, em(20.0), em(12.0), if unrec { ph::EYE_SLASH } else { ph::USER_CIRCLE }, 16.0, t.text_secondary());
        text(ctx, em(42.0), em(20.0), s.store.persona.name(), type_size::SM, Weight::Medium, t.text_primary());
        icon(ctx, em(116.0), em(13.0), ph::CARET_DOWN, 12.0, t.text_secondary());
        ctx.pop();
    }
    {
        let st2 = st.clone();
        let id = area(ctx, Rect::new(em(146.0), em(6.0), em(68.0), em(28.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| s.new_trail(false)));
        icon(ctx, em(154.0), em(12.0), ph::PLUS, 16.0, t.text_primary());
        text(ctx, em(174.0), em(20.0), "Trail", type_size::SM, Weight::Medium, t.text_primary());
        ctx.pop();
        ctx.tooltip(id, "New trail (Ctrl-T)");
    }
    let st2 = st.clone();
    icon_button(ctx, em(216.0), em(6.0), ph::SIDEBAR_SIMPLE, true, t.text_secondary(), "Collapse the rail (Ctrl-B)", move || with(&st2, |s| s.rail_collapsed = true));

    let bottom = g.h - em(8.0);
    let mut y = em(44.0);

    // "Playing" (§8a.2): another trail making sound.
    if let Some(pi) = s.store.rail_order().into_iter().find(|i| *i != s.store.current && s.store.trails[*i].row_media() == Some(RowMedia::Sound)) {
        let pt = &s.store.trails[pi];
        label(ctx, y, "PLAYING");
        y += em(18.0);
        let st2 = st.clone();
        area(ctx, Rect::new(em(8.0), y, w - em(16.0), em(32.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| s.show_trail(pi)));
        monogram(ctx, em(18.0), y + em(6.0), pt.page().map_or("", |p| &p.url), false);
        let ts = sans(type_size::SM, Weight::Regular, t.text_primary());
        text(ctx, em(46.0), y + em(16.0), &fit(pt.title(), &ts, em(160.0)), type_size::SM, Weight::Regular, t.text_primary());
        ctx.pop();
        media_button(ctx, em(212.0), y + em(4.0), pt, pi, st);
        y += em(40.0);
    }

    label(ctx, y, "TODAY");
    y += em(18.0);
    for i in s.store.rail_order() {
        if y + em(32.0) > bottom {
            break;
        }
        let tr = &s.store.trails[i];
        let current = i == s.store.current;
        let st2 = st.clone();
        let fill = if current { t.bg_elevated() } else { Color::TRANSPARENT };
        if current && s.rail_focus {
            // F6 put the keys here (§9): a focus ring on the trail.
            rect(ctx, em(8.0) - 2.0, y - 2.0, w - em(16.0) + 4.0, em(32.0) + 4.0, t.accent_mark(), em(6.0) + 2.0);
            rect(ctx, em(8.0), y, w - em(16.0), em(32.0), t.bg_surface(), em(6.0));
        }
        area(ctx, Rect::new(em(8.0), y, w - em(16.0), em(32.0)), em(6.0), fill, t.state_hover(), move || with(&st2, |s| s.show_trail(i)));
        if current {
            rect(ctx, em(8.0), y + em(8.0), 2.0, em(16.0), t.accent_mark(), 1.0);
        }
        let url = tr.page().map_or("", |p| p.url.as_str());
        if tr.page().is_some() {
            monogram(ctx, em(18.0), y + em(6.0), url, current);
        } else {
            icon(ctx, em(20.0), y + em(8.0), ph::PLUS, 16.0, t.text_tertiary());
        }
        // State glyphs on the right, right to left.
        let mut gx = em(212.0);
        let mut glyphs: Vec<(char, Color)> = vec![];
        if tr.page().is_some_and(|p| p.facts.tier == Tier::Live) {
            glyphs.push((ph::LIGHTNING, t.warning_fg()));
        }
        if tr.page().is_some_and(|p| s.store.trail_knows(i, trail::site(&p.url))) {
            glyphs.push((ph::USER, t.accent_fg()));
        }
        if tr.unrecorded {
            glyphs.push((ph::EYE_SLASH, t.text_secondary()));
        }
        let media = tr.row_media();
        if media.is_some() {
            gx -= em(26.0);
        }
        for (gch, c) in &glyphs {
            icon(ctx, gx + em(4.0), y + em(8.0), *gch, 16.0, *c);
            gx -= em(20.0);
        }
        if tr.has_unseen() && !current {
            rect(ctx, gx + em(10.0), y + em(13.0), em(6.0), em(6.0), t.text_tertiary(), em(3.0));
            gx -= em(12.0);
        }
        let w_title = gx + em(20.0) - em(46.0);
        let (wt, ct) = if current { (Weight::Semibold, t.text_primary()) } else { (Weight::Regular, t.text_primary()) };
        let title = fit(tr.title(), &sans(type_size::SM, wt, ct), w_title);
        text(ctx, em(46.0), y + em(16.0), &title, type_size::SM, wt, if tr.page().is_none() { t.text_secondary() } else { ct });
        ctx.pop();
        if media.is_some() {
            media_button(ctx, em(212.0), y + em(4.0), tr, i, st);
        }
        y += em(34.0);
        if current && tr.page().is_some() {
            y = path_rows(ctx, s, y, bottom, st);
        }
    }

    // Earlier: trails put away, by day (§4.5).
    let now_s = crate::clock::now_s();
    if !s.store.earlier.is_empty() && y + em(60.0) < bottom {
        y += em(8.0);
        label(ctx, y, "EARLIER");
        y += em(18.0);
        for (i, e) in s.store.earlier.iter().enumerate() {
            if y + em(32.0) > bottom {
                break;
            }
            let st2 = st.clone();
            let id = area(ctx, Rect::new(em(8.0), y, w - em(16.0), em(32.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| {
                s.end_edit();
                s.store.bring_back(i);
            }));
            monogram(ctx, em(18.0), y + em(6.0), e.trail.page().map_or("", |p| &p.url), false);
            let ts = sans(type_size::SM, Weight::Regular, t.text_secondary());
            let ds = sans(type_size::XXS, Weight::Regular, t.text_tertiary());
            let day = crate::clock::day_label(e.at_s, now_s);
            // The title ends a gap before the day.
            let title_max = em(228.0) - width(&day, &ds) - em(8.0) - em(46.0);
            text(ctx, em(46.0), y + em(16.0), &fit(e.trail.title(), &ts, title_max), type_size::SM, Weight::Regular, t.text_secondary());
            text(ctx, em(228.0) - width(&day, &ds), y + em(16.0), &day, type_size::XXS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            ctx.tooltip(id, format!("Put away {} · click to bring it back", crate::clock::when_label(e.at_s, now_s)));
            y += em(32.0);
        }
    }
}

fn label(ctx: &mut Ctx, y: f32, s: &str) {
    let t = ctx.theme;
    text(ctx, em(16.0), y + em(6.0), s, type_size::XXS, Weight::Semibold, t.text_tertiary());
}

/// The media glyph: a click mutes or unmutes the trail without switching
/// to it (§8a.2). The volume pop-up on right-click comes with step 6.
fn media_button(ctx: &mut Ctx, x: f32, y: f32, tr: &trail::Trail, i: usize, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let Some(m) = tr.row_media() else { return };
    let st2 = st.clone();
    let id = area(ctx, Rect::new(x, y, em(24.0), em(24.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| s.toggle_mute(i)));
    icon(ctx, x + em(4.0), y + em(4.0), media_glyph(m), 16.0, t.text_secondary());
    ctx.pop();
    ctx.tooltip(id, if m == RowMedia::Muted { "Unmute this trail" } else { "Mute this trail (Ctrl-M)" });
}

/// The current trail's path, then the current page's branches (§5.2).
fn path_rows(ctx: &mut Ctx, s: &Shell, mut y: f32, bottom: f32, st: &Mutable<Shell>) -> f32 {
    let t = ctx.theme;
    let tr = s.store.trail();
    let r = tr.path_rows();
    let top = y;
    let rows: Vec<(usize, f32)> = r.path.iter().map(|p| (*p, 0.0)).chain(r.branches.iter().map(|b| (*b, em(14.0)))).collect();
    let mut first_branch_y = None;
    let mut last_y = y;
    for (k, (pi, indent)) in rows.iter().enumerate() {
        if y + em(26.0) > bottom {
            break;
        }
        if k == r.path.len() {
            first_branch_y = Some(y);
        }
        let p = &tr.pages[*pi];
        let current = tr.current == Some(*pi);
        let x = em(46.0) + indent;
        let pi = *pi;
        let st2 = st.clone();
        area(ctx, Rect::new(x - em(6.0), y, em(228.0) - x + em(6.0), em(26.0)), em(4.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| {
            s.end_edit();
            s.store.trail_mut().go(pi);
        }));
        rect(ctx, x - em(10.0), y + em(13.0), em(8.0), 1.0, t.border_strong(), 0.0);
        let max = em(214.0) - x - em(4.0);
        if let Some(q) = &p.search {
            icon(ctx, x, y + em(6.0), ph::MAGNIFYING_GLASS, 14.0, t.text_tertiary());
            let label = format!("Search: {q}");
            let ts = sans(type_size::XS, Weight::Regular, t.text_secondary());
            text(ctx, x + em(18.0), y + em(13.0), &fit(&label, &ts, max - em(18.0)), type_size::XS, Weight::Regular, t.text_secondary());
        } else {
            let (wt, c) = if current { (Weight::Semibold, t.text_primary()) } else { (Weight::Regular, t.text_secondary()) };
            text(ctx, x, y + em(13.0), &fit(&p.title, &sans(type_size::XS, wt, c), max), type_size::XS, wt, c);
        }
        if current {
            rect(ctx, em(220.0), y + em(10.0), em(6.0), em(6.0), t.accent_mark(), em(3.0));
        } else if !p.seen {
            rect(ctx, em(220.0), y + em(10.0), em(6.0), em(6.0), t.text_tertiary(), em(3.0));
        }
        ctx.pop();
        last_y = y;
        y += em(26.0);
    }
    // The tree's lines: the path's spine, and the branches' own.
    let spine_end = first_branch_y.map_or(last_y, |b| b - em(26.0));
    if spine_end > top {
        rect(ctx, em(36.0), top + 2.0, 1.0, spine_end - top + em(11.0), t.border_strong(), 0.0);
    }
    if let Some(b) = first_branch_y {
        rect(ctx, em(50.0), b - em(6.0), 1.0, last_y - b + em(19.0), t.border_strong(), 0.0);
    }
    if r.folded > 0 && y + em(22.0) < bottom {
        let more = if r.folded == 1 { "1 more branch".to_string() } else { format!("{} more branches", r.folded) };
        text(ctx, em(60.0), y + em(9.0), &more, type_size::XXS, Weight::Medium, t.text_tertiary());
        y += em(22.0);
    }
    y + em(4.0)
}

/// The rail collapsed to monograms (Ctrl-B), the dock rail's width.
fn rail_narrow(ctx: &mut Ctx, s: &Shell, g: &Geo, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let w = g.rail;
    rect(ctx, 0.0, 0.0, w, g.h, t.bg_surface(), 0.0);
    rect(ctx, w, 0.0, 1.0, g.h, t.border_default(), 0.0);
    let st2 = st.clone();
    icon_button(ctx, em(14.0), em(6.0), ph::SIDEBAR_SIMPLE, true, t.text_secondary(), "Expand the rail (Ctrl-B)", move || with(&st2, |s| s.rail_collapsed = false));
    let st2 = st.clone();
    icon_button(ctx, em(14.0), em(44.0), ph::PLUS, true, t.text_primary(), "New trail (Ctrl-T)", move || with(&st2, |s| s.new_trail(false)));
    let mut y = em(84.0);
    for i in s.store.rail_order() {
        if y + em(36.0) > g.h {
            break;
        }
        let tr = &s.store.trails[i];
        let current = i == s.store.current;
        let st2 = st.clone();
        let id = area(ctx, Rect::new(em(8.0), y, em(40.0), em(32.0)), em(6.0), if current { t.bg_elevated() } else { Color::TRANSPARENT }, t.state_hover(), move || with(&st2, |s| s.show_trail(i)));
        if current {
            rect(ctx, em(8.0), y + em(8.0), 2.0, em(16.0), t.accent_mark(), 1.0);
        }
        match tr.page() {
            Some(p) => monogram(ctx, em(18.0), y + em(6.0), &p.url, current),
            None => {
                icon(ctx, em(20.0), y + em(8.0), ph::MAGNIFYING_GLASS, 16.0, t.text_tertiary());
            }
        }
        if tr.row_media().is_some() || tr.page().is_some_and(|p| p.facts.tier == Tier::Live) {
            let c = if tr.row_media().is_some() { t.accent_mark() } else { t.warning_fg() };
            rect(ctx, em(40.0), y + em(4.0), em(6.0), em(6.0), c, em(3.0));
        }
        ctx.pop();
        ctx.tooltip(id, tr.title().to_string());
        y += em(36.0);
    }
}

// -- the toolbar --------------------------------------------------------------

fn toolbar(ctx: &mut Ctx, s: &Shell, g: &Geo, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let x0 = g.x0;
    rect(ctx, x0, 0.0, g.w - x0, shell::bar(), t.bg_surface(), 0.0);
    rect(ctx, x0, shell::bar(), g.w - x0, 1.0, t.border_default(), 0.0);
    let tr = s.store.trail();
    let page = tr.page();
    let st2 = st.clone();
    icon_button(ctx, x0 + em(9.0), em(6.0), ph::ARROW_LEFT, tr.can_back(), t.text_primary(), "Back (Alt-Left)", move || with(&st2, |s| s.apply(crate::keys::Action::Back, now_ms())));
    let st2 = st.clone();
    icon_button(ctx, x0 + em(41.0), em(6.0), ph::ARROW_RIGHT, tr.can_forward(), t.text_secondary(), "Forward (Alt-Right)", move || with(&st2, |s| s.apply(crate::keys::Action::Forward, now_ms())));
    let st2 = st.clone();
    icon_button(ctx, x0 + em(73.0), em(6.0), ph::ARROW_CLOCKWISE, page.is_some(), t.text_secondary(), "Reload: the current version of this page (Ctrl-R)", move || with(&st2, |s| s.reload()));

    let f = g.where_;
    match &s.editing {
        Some(e) => where_editing(ctx, f, e),
        None => where_at_rest(ctx, s, f, st),
    }

    // Media (when this page plays), presence, then Aa, Keep, More.
    let px = f.x() + f.w() + em(12.0);
    if g.media {
        let m = tr.row_media().unwrap_or(RowMedia::Sound);
        let i = s.store.current;
        let st2 = st.clone();
        icon_button(ctx, px - 2.0, em(6.0), media_glyph(m), true, t.text_secondary(), "Mute this trail (Ctrl-M)", move || with(&st2, |s| s.toggle_mute(i)));
    }
    if let Some(p) = page {
        let site = trail::site(&p.url).to_string();
        let knows = s.store.knows(&site);
        let st2 = st.clone();
        let (fill, hover) = if knows { (t.accent_bg(), t.accent_bg()) } else { (t.bg_surface(), t.state_hover()) };
        let edge = if knows { t.accent_mark() } else { t.border_default() };
        let p = g.presence;
        let (y, h, mid) = (p.y(), p.h(), p.y() + p.h() / 2.0);
        rect(ctx, p.x(), y, p.w(), h, edge, h / 2.0);
        let id = area(ctx, Rect::new(p.x() + 1.0, y + 1.0, p.w() - 2.0, h - 2.0), h / 2.0 - 1.0, fill, hover, move || with(&st2, |s| s.toggle_presence_menu()));
        let (gx, tx) = (p.x() + em(9.0), p.x() + em(29.0));
        if knows {
            icon(ctx, gx, mid - em(8.0), ph::USER, 16.0, t.accent_strong());
            let ts = sans(type_size::XS, Weight::Semibold, t.accent_strong());
            text(ctx, tx, mid, &fit(&site, &ts, p.x() + p.w() - em(7.0) - tx), type_size::XS, Weight::Semibold, t.accent_strong());
        } else {
            icon(ctx, gx, mid - em(8.0), ph::EYE_SLASH, 16.0, t.text_secondary());
            let ts = sans(type_size::XS, Weight::Medium, t.text_secondary());
            text(ctx, tx, mid, &fit("Anonymous", &ts, p.x() + p.w() - em(7.0) - tx), type_size::XS, Weight::Medium, t.text_secondary());
        }
        ctx.pop();
        ctx.tooltip(id, if knows { format!("{site} knows you") } else { format!("{site} doesn't know you") });
    }
    let has = page.is_some();
    let st2 = st.clone();
    let aa = s.reading_open;
    icon_button(ctx, g.aa.x(), g.aa.y(), ph::TEXT_AA, has, if aa { t.accent_fg() } else { t.text_secondary() }, "Reading controls", move || with(&st2, |s| {
        s.popover = false;
        s.presence_open = false;
        s.reading_open = !aa;
    }));
    let kept = page.is_some_and(|p| s.store.is_kept(&p.url));
    let st2 = st.clone();
    icon_button(ctx, g.w - em(72.0), em(6.0), ph::STAR, has, if kept { t.accent_fg() } else { t.text_secondary() }, if kept { "Kept · click to stop keeping" } else { "Keep this page (Ctrl-D)" }, move || with(&st2, |s| s.store.toggle_keep()));
    icon_button(ctx, g.w - em(40.0), em(6.0), ph::DOTS_THREE, true, t.text_secondary(), "More", || {});
}

/// The Where field at rest (§5.3): the tier chip with its light, the
/// title, then the address with the registrable domain emphasised.
fn where_at_rest(ctx: &mut Ctx, s: &Shell, f: Rect, st: &Mutable<Shell>) {
    let t = ctx.theme;
    rect(ctx, f.x(), f.y(), f.w(), f.h(), t.border_default(), 6.0);
    let st2 = st.clone();
    area(ctx, Rect::new(f.x() + 1.0, f.y() + 1.0, f.w() - 2.0, f.h() - 2.0), 5.0, t.bg_canvas(), t.bg_canvas(), move || with(&st2, |s| s.begin_edit()));
    let Some(p) = s.store.trail().page() else {
        icon(ctx, f.x() + em(10.0), f.y() + f.h() / 2.0 - em(7.0), ph::MAGNIFYING_GLASS, 14.0, t.text_tertiary());
        text(ctx, f.x() + em(30.0), f.y() + f.h() / 2.0, "Search or type an address", type_size::SM, Weight::Regular, t.text_tertiary());
        ctx.pop();
        return;
    };
    let cw = tier_chip(ctx, s, f.x() + em(4.0), f.y() + (f.h() - em(22.0)) / 2.0, p, st);
    let x = f.x() + em(4.0) + cw + em(10.0);
    let end = f.x() + f.w() - em(10.0);
    let body = |c: Color, w: Weight| sans(type_size::SM, w, c);
    let mut runs: Vec<(String, TextStyle)> = vec![];
    if p.facts.insecure {
        runs.push(("Not private".into(), body(t.warning_fg(), Weight::Semibold)));
    }
    let title_st = body(t.text_primary(), Weight::Medium);
    let (before, domain, after) = trail::split_address(&p.url);
    let addr_w = width(before, &body(t.text_tertiary(), Weight::Regular)) + width(domain, &body(t.text_primary(), Weight::Semibold));
    let room = end - x;
    // The title gets what the domain leaves; the domain is never cut.
    let title_max = (room - addr_w - em(40.0)).max(em(60.0)).min(room * 0.55);
    let title = if p.search.is_some() { p.title.clone() } else { fit(&p.title, &title_st, title_max) };
    let used: f32 = runs.iter().map(|r| width(&r.0, &r.1) + em(8.0)).sum::<f32>() + width(&title, &title_st) + em(16.0) + addr_w;
    runs.push((title, title_st));
    runs.push(("·".into(), body(t.text_tertiary(), Weight::Regular)));
    let r = ctx.push(Node::stack(Axis::Horizontal, Rect::new(x, f.y() + f.h() / 2.0 - em(type_size::SM) * 0.75, 0.0, 0.0), em(8.0)));
    ctx.set_flex(r, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
    for (sx, sst) in &runs {
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: sx.clone(), style: sst.clone() });
    }
    let a = ctx.push(Node::stack(Axis::Horizontal, Rect::ZERO_SIZED, 0.0));
    ctx.set_flex(a, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
    if !before.is_empty() {
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: before.into(), style: body(t.text_tertiary(), Weight::Regular) });
    }
    ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: domain.into(), style: body(t.text_primary(), Weight::Semibold) });
    let path_st = body(t.text_tertiary(), Weight::Regular);
    let path = fit(after, &path_st, room - used);
    if !path.is_empty() {
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: path, style: path_st });
    }
    ctx.pop();
    ctx.pop();
    ctx.pop();
}

/// The tier chip (§6.3) with the light before the word (§6.5). Returns
/// its width. A click opens the popover.
fn tier_chip(ctx: &mut Ctx, s: &Shell, x: f32, y: f32, p: &trail::Page, st: &Mutable<Shell>) -> f32 {
    let t = ctx.theme;
    let f = &p.facts;
    let segs: Vec<(char, String, bool)> = match f.tier {
        Tier::Static => vec![(ph::FILE_TEXT, "Static".into(), false)],
        Tier::Replayed => vec![(ph::FILM_STRIP, "Replayed".into(), false)],
        Tier::Live => vec![(ph::LIGHTNING, "Live code".into(), true)],
    }
    .into_iter()
    .chain((!f.live_parts.is_empty() && f.tier != Tier::Live).then(|| {
        let n = f.live_parts.len();
        (ph::LIGHTNING, if n == 1 { "1 live part".to_string() } else { format!("{n} live parts") }, true)
    }))
    .collect();
    let ls = sans(type_size::XS, Weight::Semibold, t.text_secondary());
    let total: f32 = em(24.0) + segs.iter().map(|(_, l, _)| width(l, &ls) + em(36.0) + 4.0).sum::<f32>();
    let ch = em(22.0);
    let st2 = st.clone();
    let open = s.popover;
    let id = area(ctx, Rect::new(x, y, total, ch), ch / 2.0, Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| {
        s.popover = !open;
        s.end_edit();
    }));
    icon(ctx, x + em(4.0), y + (ch - em(16.0)) / 2.0, light_glyph(f.light), 16.0, light_color(ctx, f.light));
    let mut sx = x + em(24.0);
    for (g, l, live) in &segs {
        let w = width(l, &ls) + em(36.0);
        let (edge, fill, fg) = if *live { (t.warning_fg(), t.bg_canvas(), t.warning_fg()) } else { (t.border_strong(), t.bg_surface(), t.text_secondary()) };
        boxed(ctx, sx, y, w, ch, edge, fill, ch / 2.0);
        icon(ctx, sx + em(8.0), y + (ch - em(14.0)) / 2.0, *g, 14.0, fg);
        text(ctx, sx + em(26.0), y + ch / 2.0, l, type_size::XS, Weight::Semibold, fg);
        sx += w + 4.0;
    }
    ctx.pop();
    if !open {
        ctx.tooltip(id, match f.light {
            Light::Green => "Nothing to worry about that we can see · click for details".to_string(),
            Light::Amber => format!("{} · click for details", f.reasons.first().map_or("Something is weaker than usual", |r| r.as_str())),
            Light::Red => "This page was stopped".to_string(),
        });
    }
    total
}

/// The Where field focused: a text field with the full address (§5.3).
fn where_editing(ctx: &mut Ctx, f: Rect, e: &shell::WhereEdit) {
    text_field(ctx, f, &e.edit, "Search or type an address");
}

/// A focused one-line field: ring, box, magnifier, text with its
/// selection and caret, or the placeholder.
fn text_field(ctx: &mut Ctx, f: Rect, edit: &pergola::text_edit::TextEdit, placeholder: &str) {
    let t = ctx.theme;
    boxed(ctx, f.x() - 2.0, f.y() - 2.0, f.w() + 4.0, f.h() + 4.0, t.focus_ring(), t.focus_ring(), em(8.0));
    boxed(ctx, f.x(), f.y(), f.w(), f.h(), t.border_control(), t.bg_canvas(), em(6.0));
    icon(ctx, f.x() + em(10.0), f.y() + (f.h() - em(14.0)) / 2.0, ph::MAGNIFYING_GLASS, 14.0, t.text_tertiary());
    let st = sans(type_size::SM, Weight::Regular, t.text_primary());
    let tx = f.x() + em(30.0);
    let s = edit.text();
    let cy = f.y() + f.h() / 2.0;
    let line = Node::measure_text("Mg", &st).h;
    let sel = edit.selection();
    if !sel.is_empty() {
        let a = tx + width(&s[..sel.start], &st);
        let b = tx + width(&s[..sel.end], &st);
        rect(ctx, a, cy - line / 2.0, (b - a).min(f.x() + f.w() - em(8.0) - a), line, t.selection_bg(), 0.0);
    }
    if s.is_empty() {
        text(ctx, tx, cy, placeholder, type_size::SM, Weight::Regular, t.text_tertiary());
    } else {
        text(ctx, tx, cy, &fit(s, &st, f.w() - em(40.0)), type_size::SM, Weight::Regular, t.text_primary());
    }
    if sel.is_empty() {
        let cx = tx + width(&s[..edit.caret()], &st);
        rect(ctx, cx, cy - line / 2.0, 1.5, line, t.accent_fg(), 0.0);
    }
}

/// The suggestions (§5.3), drawn in the window where the real ones are a
/// Fresco pop-up. Local sources only; the search is sent only on Enter.
fn suggestions(ctx: &mut Ctx, s: &Shell, r: Rect, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let v = s.suggestions();
    let sel = s.editing.as_ref().and_then(|e| e.selected);
    boxed(ctx, r.x(), r.y(), r.w(), r.h(), t.border_default(), t.bg_elevated(), em(8.0));
    let body = |c: Color, w: Weight| sans(type_size::SM, w, c);
    let mut y = r.y() + em(8.0);
    let mut last: Option<u8> = None;
    for (i, sg) in v.iter().enumerate() {
        let group = match sg.source {
            Source::Open { .. } => 0,
            Source::Kept | Source::History => 1,
            Source::Search => 2,
        };
        if last != Some(group) {
            if group == 2 {
                rect(ctx, r.x() + 1.0, y + em(3.0), r.w() - 2.0, 1.0, t.border_default(), 0.0);
                y += em(9.0);
            } else {
                text(ctx, r.x() + em(14.0), y + em(10.0), if group == 0 { "OPEN TRAILS" } else { "KEPT AND HISTORY" }, type_size::XXS, Weight::Semibold, t.text_tertiary());
                y += em(DROP_LABEL);
            }
        }
        last = Some(group);
        let st2 = st.clone();
        let sg2 = sg.clone();
        let fill = if sel == Some(i) { t.state_hover() } else { Color::TRANSPARENT };
        area(ctx, Rect::new(r.x() + em(6.0), y, r.w() - em(12.0), em(DROP_ROW)), em(6.0), fill, t.state_hover(), move || with(&st2, |s| s.take_suggestion(&sg2, now_ms())));
        let cy = y + em(DROP_ROW) / 2.0;
        let room = r.w() - em(60.0);
        match &sg.source {
            Source::Open { trail_title, .. } => {
                monogram(ctx, r.x() + em(14.0), y + em(5.0), &sg.url, true);
                let a = fit(&sg.title, &body(t.text_primary(), Weight::Medium), room * 0.45);
                let b = fit(&format!("in trail “{trail_title}”"), &body(t.text_tertiary(), Weight::Regular), room * 0.4);
                inline(ctx, r.x() + em(44.0), cy, em(10.0), &[(a, body(t.text_primary(), Weight::Medium)), (b, body(t.text_tertiary(), Weight::Regular))]);
                let ss = sans(type_size::XS, Weight::Medium, t.accent_fg());
                text(ctx, r.x() + r.w() - em(18.0) - width("Switch to", &ss), cy, "Switch to", type_size::XS, Weight::Medium, t.accent_fg());
            }
            Source::Kept | Source::History => {
                let g = if sg.source == Source::Kept { ph::BOOKMARK } else { ph::CLOCK };
                icon(ctx, r.x() + em(16.0), y + em(7.0), g, 16.0, t.text_tertiary());
                let addr = sg.url.split_once("://").map_or(sg.url.as_str(), |(_, r)| r);
                let a = fit(&sg.title, &body(t.text_primary(), Weight::Regular), room * 0.5);
                let b = fit(addr, &body(t.text_tertiary(), Weight::Regular), room * 0.45);
                inline(ctx, r.x() + em(44.0), cy, em(10.0), &[(a, body(t.text_primary(), Weight::Regular)), (b, body(t.text_tertiary(), Weight::Regular))]);
            }
            Source::Search => {
                icon(ctx, r.x() + em(16.0), y + em(7.0), ph::MAGNIFYING_GLASS, 16.0, t.text_tertiary());
                let a = fit(&format!("Search for “{}”", sg.title), &body(t.text_primary(), Weight::Regular), room * 0.55);
                inline(ctx, r.x() + em(44.0), cy, em(6.0), &[(a, body(t.text_primary(), Weight::Regular)), ("· sent only when you press Enter".into(), body(t.text_tertiary(), Weight::Regular))]);
            }
        }
        ctx.pop();
        y += em(DROP_ROW);
    }
}

/// The tier chip's popover (§6.5): Connection, What runs, the reasons,
/// and the line that says what green does not mean.
fn popover(ctx: &mut Ctx, r: Rect, p: &trail::Page) {
    let t = ctx.theme;
    boxed(ctx, r.x(), r.y(), r.w(), r.h(), t.border_default(), t.bg_elevated(), em(8.0));
    let f = &p.facts;
    let site = trail::site(&p.url);
    let x = r.x() + em(16.0);
    let w = r.w() - em(32.0);
    let mut y = r.y() + em(14.0);
    icon(ctx, x, y, light_glyph(f.light), 16.0, light_color(ctx, f.light));
    let head = match f.light {
        Light::Green => "Nothing to worry about that we can see",
        Light::Amber => "Something is weaker than usual",
        Light::Red => "This page is unsafe",
    };
    text(ctx, x + em(22.0), y + em(8.0), head, type_size::SM, Weight::Semibold, t.text_primary());
    y += em(28.0);
    text(ctx, x, y + em(6.0), "CONNECTION", type_size::XXS, Weight::Semibold, t.text_tertiary());
    y += em(16.0);
    let conn = shell::popover_connection(f, site);
    y = wrapped(ctx, x, y, w, &conn, t.text_secondary());
    y += em(6.0);
    text(ctx, x, y + em(6.0), "WHAT RUNS", type_size::XXS, Weight::Semibold, t.text_tertiary());
    y += em(16.0);
    y = wrapped(ctx, x, y, w, &tier_sentence(f.tier, site), t.text_secondary());
    for o in &f.live_parts {
        y = wrapped(ctx, x, y, w, &shell::popover_live_part(o), t.warning_fg());
    }
    for reason in &f.reasons {
        y = wrapped(ctx, x, y, w, reason, if f.light == Light::Red { t.danger_fg() } else { t.warning_fg() });
    }
    y += em(6.0);
    wrapped(ctx, x, y, w, shell::popover_last(f), t.text_tertiary());
}

/// The presence menu (§6.2): whether the site knows you, the accounts Arca
/// holds for it, and what you can do about it.
fn presence_menu(ctx: &mut Ctx, r: Rect, site: &str, knows: bool, accounts: Option<shell::Accounts>, st: &Mutable<Shell>) {
    let t = ctx.theme;
    boxed(ctx, r.x(), r.y(), r.w(), r.h(), t.border_default(), t.bg_elevated(), em(8.0));
    let x = r.x() + em(16.0);
    let w = r.w() - em(32.0);
    let mut y = r.y() + em(14.0);
    let head_st = sans(type_size::SM, Weight::Semibold, t.text_primary());
    icon(ctx, x, y, if knows { ph::USER } else { ph::EYE_SLASH }, 16.0, if knows { t.accent_strong() } else { t.text_secondary() });
    let head = if knows { format!("{site} knows you") } else { format!("{site} doesn't know you") };
    text(ctx, x + em(22.0), y + em(8.0), &fit(&head, &head_st, w - em(22.0)), type_size::SM, Weight::Semibold, t.text_primary());
    y += em(28.0);
    // A row of the menu: a glyph and its words, the whole width pressable.
    let row = |ctx: &mut Ctx, y: f32, g: char, label: &str, c: Color, press: Box<dyn Fn()>| {
        let t = ctx.theme;
        area(ctx, Rect::new(r.x() + em(6.0), y, r.w() - em(12.0), em(32.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || press());
        icon(ctx, x, y + em(8.0), g, 16.0, c);
        text(ctx, x + em(24.0), y + em(16.0), label, type_size::SM, Weight::Medium, c);
        ctx.pop();
    };
    if !knows {
        text(ctx, x, y + em(6.0), "Its pages open anonymously, with no cookies.", type_size::XS, Weight::Regular, t.text_secondary());
        y += em(16.0);
        let s2 = st.clone();
        row(ctx, y, ph::USER, "Let this site remember me", t.text_primary(), Box::new(move || with(&s2, |s| s.presence_choose(true))));
        return;
    }
    text(ctx, x, y + em(6.0), "ACCOUNT IN ARCA", type_size::XXS, Weight::Semibold, t.text_tertiary());
    y += em(16.0);
    let line_st = sans(type_size::XS, Weight::Regular, t.text_secondary());
    let mut line = |ctx: &mut Ctx, g: Option<char>, s: &str, c: Color| {
        let dx = if let Some(g) = g {
            icon(ctx, x, y + em(2.0), g, 14.0, c);
            em(20.0)
        } else { 0.0 };
        text(ctx, x + dx, y + em(10.0), &fit(s, &line_st, w - dx), type_size::XS, Weight::Regular, c);
        y += em(20.0);
    };
    match accounts {
        Some(shell::Accounts::Known(v)) if !v.is_empty() => {
            for a in v.iter().take(4) {
                let (g, kind) = match a.kind {
                    navigator_control::AccountKind::Passkey => (ph::KEY, "passkey"),
                    navigator_control::AccountKind::Password => (ph::PASSWORD, "password"),
                };
                line(ctx, Some(g), &format!("{} · {kind}", a.name), t.text_primary());
            }
        }
        Some(shell::Accounts::Known(_)) => line(ctx, None, "Arca holds no account for this site.", t.text_secondary()),
        Some(shell::Accounts::Unknown(why)) => line(ctx, None, &format!("Arca can't say ({why})."), t.text_tertiary()),
        Some(shell::Accounts::Asking) | None => line(ctx, None, "Asking Arca…", t.text_tertiary()),
    }
    y += em(8.0);
    let (s2, s3) = (st.clone(), st.clone());
    row(ctx, y, ph::SIGN_OUT, "Sign out here", t.text_primary(), Box::new(move || with(&s2, |s| s.sign_out_here())));
    y += em(34.0);
    row(ctx, y, ph::TRASH_SIMPLE, "Forget this site", t.danger_fg(), Box::new(move || with(&s3, |s| s.presence_choose(false))));
}

/// Words broken into lines of at most `w` px, by measurement; returns the
/// y below the last line.
fn wrapped(ctx: &mut Ctx, x: f32, mut y: f32, w: f32, s: &str, c: Color) -> f32 {
    for line in shell::wrap(s, w) {
        text(ctx, x, y + em(8.0), &line, type_size::XS, Weight::Regular, c);
        y += em(16.0);
    }
    y
}

// -- reading controls and find (§7) ----------------------------------------------

/// A small bordered button with a glyph or a word.
fn small_button(ctx: &mut Ctx, r: Rect, g: Option<char>, label: &str, on: bool, tip: &str, press: impl Fn() + 'static) {
    let t = ctx.theme;
    let (edge, fill, fg) = if on { (t.accent_mark(), t.accent_bg(), t.accent_strong()) } else { (t.border_control(), t.bg_surface(), t.text_primary()) };
    rect(ctx, r.x(), r.y(), r.w(), r.h(), edge, em(6.0));
    let id = area(ctx, Rect::new(r.x() + 1.0, r.y() + 1.0, r.w() - 2.0, r.h() - 2.0), em(5.0), fill, t.state_hover(), press);
    match g {
        Some(g) => {
            icon(ctx, r.x() + (r.w() - em(16.0)) / 2.0, r.y() + (r.h() - em(16.0)) / 2.0, g, 16.0, fg);
        }
        None => {
            let st = sans(type_size::XS, Weight::Medium, fg);
            let w = width(label, &st);
            text(ctx, r.x() + (r.w() - w) / 2.0, r.y() + r.h() / 2.0, label, type_size::XS, Weight::Medium, fg);
        }
    }
    ctx.pop();
    if !tip.is_empty() {
        ctx.tooltip(id, tip.to_string());
    }
}

fn percent(v: u16) -> String {
    format!("{}%", (v as u32 * 100 + 32) / 64)
}

/// The Aa pop-up (§7): text size and zoom steppers with a reset, the
/// theme, the reading width. Remembered per site and persona.
fn reading_controls(ctx: &mut Ctx, r: Rect, p: trail::ReadingPrefs, site: &str, st: &Mutable<Shell>) {
    use trail::{ReadingPrefs as R, Theme};
    let t = ctx.theme;
    boxed(ctx, r.x(), r.y(), r.w(), r.h(), t.border_default(), t.bg_elevated(), 8.0);
    let x = r.x() + em(16.0);
    let right = r.x() + r.w() - em(16.0);
    let mut y = r.y() + em(12.0);
    text(ctx, x, y + em(10.0), "Reading", type_size::SM, Weight::Semibold, t.text_primary());
    y += em(30.0);
    let stepper = |ctx: &mut Ctx, y: f32, name: &str, v: u16, f: fn(&mut R, Option<bool>)| {
        let t = ctx.theme;
        let (bw, vw) = (em(28.0), em(56.0));
        text(ctx, x, y + bw / 2.0, name, type_size::XS, Weight::Medium, t.text_secondary());
        let bx = right - em(172.0);
        let (a, b, c) = (st.clone(), st.clone(), st.clone());
        small_button(ctx, Rect::new(bx, y, bw, bw), Some(ph::MINUS), "", false, &format!("Smaller {}", name.to_lowercase()), move || with(&a, |s| s.reading_change(|r| f(r, Some(false)))));
        let vs = sans(type_size::SM, Weight::Semibold, t.text_primary());
        let label = percent(v);
        let w = width(&label, &vs);
        text(ctx, bx + bw + (vw - w) / 2.0, y + bw / 2.0, &label, type_size::SM, Weight::Semibold, t.text_primary());
        small_button(ctx, Rect::new(bx + bw + vw, y, bw, bw), Some(ph::PLUS), "", false, &format!("Larger {}", name.to_lowercase()), move || with(&b, |s| s.reading_change(|r| f(r, Some(true)))));
        small_button(ctx, Rect::new(bx + em(120.0), y, em(52.0), bw), None, "Reset", false, "", move || with(&c, |s| s.reading_change(|r| f(r, None))));
    };
    stepper(ctx, y, "Text size", p.text_64, |r, up| r.text_64 = up.map_or(64, |u| R::step(r.text_64, u)));
    y += em(38.0);
    stepper(ctx, y, "Zoom", p.zoom_64, |r, up| r.zoom_64 = up.map_or(64, |u| R::step_zoom(r.zoom_64, u)));
    y += em(42.0);
    text(ctx, x, y + em(14.0), "Theme", type_size::XS, Weight::Medium, t.text_secondary());
    let mut bx = right - em(196.0);
    for (theme, label, w) in [(Theme::System, "System", em(72.0)), (Theme::Light, "Light", em(58.0)), (Theme::Dark, "Dark", em(58.0))] {
        let s2 = st.clone();
        let tip = if theme == Theme::System { "Follow the desktop's appearance" } else { "" };
        small_button(ctx, Rect::new(bx, y, w, em(28.0)), None, label, p.theme == theme, tip, move || with(&s2, |s| s.reading_change(|r| r.theme = theme)));
        bx += w + 4.0;
    }
    y += em(38.0);
    text(ctx, x, y + em(14.0), "Width", type_size::XS, Weight::Medium, t.text_secondary());
    let mut bx = right - em(196.0);
    for (comfortable, label, w) in [(false, "Full", em(72.0)), (true, "Comfortable", em(120.0))] {
        let s2 = st.clone();
        let tip = if comfortable { "About 70 characters a line, centred" } else { "" };
        small_button(ctx, Rect::new(bx, y, w, em(28.0)), None, label, p.comfortable == comfortable, tip, move || with(&s2, |s| s.reading_change(|r| r.comfortable = comfortable)));
        bx += w + 4.0;
    }
    y += em(44.0);
    let ns = sans(type_size::XXS, Weight::Regular, t.text_tertiary());
    text(ctx, x, y + em(6.0), &fit(&format!("Remembered for {site} in this persona."), &ns, r.w() - em(32.0)), type_size::XXS, Weight::Regular, t.text_tertiary());
}

/// The find bar (§7), under the toolbar: the field, "n of m" as the page
/// worker counts them, previous and next, close.
fn find_bar(ctx: &mut Ctx, r: Rect, f: &shell::FindBar, st: &Mutable<Shell>) {
    let t = ctx.theme;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_surface(), 0.0);
    rect(ctx, r.x(), r.y() + r.h() - 1.0, r.w(), 1.0, t.border_default(), 0.0);
    let fw = em(320.0).min(r.w() - em(220.0)).max(em(120.0));
    text_field(ctx, Rect::new(r.x() + em(12.0), r.y() + em(6.0), fw, em(28.0)), &f.edit, "Find in page");
    let x = r.x() + em(24.0) + fw;
    let what = if f.edit.text().is_empty() {
        String::new()
    } else if !f.answered {
        "…".into()
    } else if f.count == 0 {
        "No matches".into()
    } else if f.count >= limen::doc_viewer::FOUND_MAX {
        format!("{} of {}+", f.current, f.count)
    } else {
        format!("{} of {}", f.current, f.count)
    };
    let c = if f.answered && f.count == 0 && !f.edit.text().is_empty() { t.danger_fg() } else { t.text_secondary() };
    text(ctx, x, r.y() + em(20.0), &what, type_size::XS, Weight::Medium, c);
    let has = f.count > 0;
    let (a, b, c2) = (st.clone(), st.clone(), st.clone());
    let enter = |up: bool| pergola::Modifiers { shift: up, ..Default::default() };
    icon_button(ctx, x + em(96.0), r.y() + em(6.0), ph::CARET_UP, has, t.text_secondary(), "Previous match (Shift-Enter)", move || with(&a, |s| { s.find_key(pergola::Key::Enter, enter(true), ""); }));
    icon_button(ctx, x + em(126.0), r.y() + em(6.0), ph::CARET_DOWN, has, t.text_secondary(), "Next match (Enter)", move || with(&b, |s| { s.find_key(pergola::Key::Enter, enter(false), ""); }));
    icon_button(ctx, r.x() + r.w() - em(40.0), r.y() + em(6.0), ph::X, true, t.text_secondary(), "Close (Esc)", move || with(&c2, |s| s.close_find()));
}

// -- the history view (§5.5) ---------------------------------------------------------

/// Ctrl-H: every page of the open trails and of Earlier, newest first,
/// by day; a filter; a press goes to the page, in its trail.
fn history(ctx: &mut Ctx, s: &Shell, g: &Geo, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let r = g.page;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_canvas(), 0.0);
    let Some(h) = &s.history else { return };
    let col = em(720.0).min(r.w() - em(64.0));
    let x = r.x() + ((r.w() - col) / 2.0).max(em(32.0));
    let mut y = r.y() + em(24.0);
    text(ctx, x, y + em(12.0), "History", type_size::XL, Weight::Semibold, t.text_primary());
    let st2 = st.clone();
    icon_button(ctx, x + col - em(28.0), y, ph::X, true, t.text_secondary(), "Close (Esc)", move || with(&st2, |s| s.history = None));
    let st2 = st.clone();
    let (cx, cw) = shell::history_clear_all(x, col);
    small_button(ctx, Rect::new(cx, y + 1.0, cw, em(26.0)), None, "Clear all…", false, "Clear all history", move || with(&st2, |s| s.ask_clear(Clear::All)));
    y += em(40.0);
    text_field(ctx, Rect::new(x, y, col, em(32.0)), &h.edit, "Filter by title or address");
    y += em(48.0);
    if let Some((what, n)) = &h.confirm {
        // It can't be undone: it says what it takes, and asks.
        boxed(ctx, x - em(8.0), y, col + em(16.0), em(shell::HIST_CONFIRM) - em(12.0), t.border_default(), t.bg_elevated(), em(8.0));
        text(ctx, x + em(4.0), y + em(22.0), &fit(&clear_question(what, *n), &sans(type_size::SM, Weight::Medium, t.text_primary()), col - em(180.0)), type_size::SM, Weight::Medium, t.text_primary());
        let st2 = st.clone();
        small_button(ctx, Rect::new(x + col - em(160.0), y + em(8.0), em(72.0), em(28.0)), None, "Cancel", false, "", move || with(&st2, |s| {
            if let Some(h) = &mut s.history {
                h.confirm = None;
            }
        }));
        let st2 = st.clone();
        small_button(ctx, Rect::new(x + col - em(80.0), y + em(8.0), em(80.0), em(28.0)), None, "Clear", true, "", move || with(&st2, |s| s.confirm_clear()));
        y += em(shell::HIST_CONFIRM);
    }
    let rows = s.history_rows();
    if rows.is_empty() {
        let msg = if h.edit.text().is_empty() { "Nothing yet. Pages you open in recorded trails are listed here." } else { "No page matches." };
        text(ctx, x, y + em(10.0), msg, type_size::SM, Weight::Regular, t.text_secondary());
        return;
    }
    let now = crate::clock::now_s();
    let mut day = String::new();
    let ts = sans(type_size::SM, Weight::Medium, t.text_primary());
    let us = TextStyle { family: font::MONO.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() };
    for (i, row) in rows.iter().enumerate() {
        let d = crate::history::day_of(row.at_s, now);
        if d != day {
            if y + em(HIST_DAY) + em(shell::HIST_ROW) > r.y() + r.h() {
                break;
            }
            text(ctx, x, y + em(16.0), &d.to_uppercase(), type_size::XXS, Weight::Semibold, t.text_tertiary());
            let st2 = st.clone();
            let d2 = d.clone();
            small_button(ctx, Rect::new(x + col - em(shell::HIST_DAY_CLEAR_W), y + em(3.0), em(shell::HIST_DAY_CLEAR_W), em(22.0)), None, "Clear day", false, &format!("Clear {d}"), move || with(&st2, |s| s.ask_clear(Clear::Day(d2.clone()))));
            y += em(shell::HIST_DAY);
            day = d;
        }
        if y + em(shell::HIST_ROW) > r.y() + r.h() {
            break;
        }
        let st2 = st.clone();
        let row2 = row.clone();
        let fill = if i == h.selected { t.state_hover() } else { Color::TRANSPARENT };
        area(ctx, Rect::new(x - em(8.0), y, col + em(16.0), em(shell::HIST_ROW)), em(6.0), fill, t.state_hover(), move || with(&st2, |s| s.open_history_row(&row2, now_ms())));
        monogram(ctx, x, y + em(7.0), &row.url, false);
        text(ctx, x + em(30.0), y + em(17.0), &fit(&row.title, &ts, col * 0.45), type_size::SM, Weight::Medium, t.text_primary());
        let ux = x + em(40.0) + col * 0.45;
        let when = if row.at_s == 0 { String::new() } else { crate::clock::when_label(row.at_s, now).rsplit(' ').next().unwrap_or("").to_string() };
        let label = match row.place {
            crate::history::Place::Open(_) => when,
            crate::history::Place::Earlier(_) => format!("put away · {when}"),
        };
        let ls = sans(type_size::XS, Weight::Regular, t.text_tertiary());
        let lw = width(&label, &ls);
        // The address up to the time label and the clear button.
        ctx.add(Node::Text { rect: Rect::new(ux, y + em(9.0), 0.0, em(16.0)), content: fit(&row.url, &us, (col - (ux - x) - lw - em(36.0) - em(16.0)).max(em(40.0))), style: us.clone() });
        text(ctx, x + col - lw - em(36.0), y + em(17.0), &label, type_size::XS, Weight::Regular, t.text_tertiary());
        ctx.pop();
        let st2 = st.clone();
        let site = trail::site(&row.url).to_string();
        icon_button(ctx, x + col - em(28.0), y + em(3.0), ph::TRASH_SIMPLE, true, t.text_tertiary(), &format!("Clear everything from {site}"), move || with(&st2, |s| s.ask_clear(Clear::Site(site.clone()))));
        y += em(shell::HIST_ROW);
    }
}

/// What the clear confirmation asks.
pub fn clear_question(what: &Clear, n: usize) -> String {
    let pages = if n == 1 { "1 page".to_string() } else { format!("{n} pages") };
    match (what, n) {
        (_, 0) => "Nothing to clear here: the page each trail is on stays.".into(),
        (Clear::All, _) => format!("Clear all history, {pages} with their stored versions? This can't be undone."),
        _ => format!("Clear {pages} {}, with their stored versions? This can't be undone.", what.label()),
    }
}

// -- the prompt row ---------------------------------------------------------------

/// Asked by the Navigator, between the toolbar and the page, pushing the
/// page down (§6.4). Never asks for a secret.
fn prompt_row(ctx: &mut Ctx, r: Rect, p: &Prompt, st: &Mutable<Shell>) {
    let t = ctx.theme;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_elevated(), 0.0);
    rect(ctx, r.x(), r.y() + r.h() - 1.0, r.w(), 1.0, t.border_default(), 0.0);
    let body = |c: Color, w: Weight| sans(type_size::SM, w, c);
    let (g, runs): (char, Vec<(String, TextStyle)>) = match p {
        Prompt::Download { file, size, origin, kind, .. } => (ph::DOWNLOAD, vec![
            (file.clone(), body(t.text_primary(), Weight::Semibold)),
            (format!("{size} {kind} from"), body(t.text_secondary(), Weight::Regular)),
            (origin.clone(), body(t.text_primary(), Weight::Semibold)),
        ]),
        Prompt::Handoff { url, scheme } => (ph::ARROW_RIGHT, vec![
            (format!("This link is for another program ({scheme}):"), body(t.text_secondary(), Weight::Regular)),
            (url.trim_start_matches(&format!("{scheme}:")).chars().take(80).collect(), body(t.text_primary(), Weight::Semibold)),
        ]),
        Prompt::OpenApp { name, .. } => (ph::ARROW_RIGHT, vec![
            ("This link opens the app".into(), body(t.text_secondary(), Weight::Regular)),
            (name.chars().take(80).collect(), body(t.text_primary(), Weight::Semibold)),
            ("· Try it keeps nothing".into(), body(t.text_secondary(), Weight::Regular)),
        ]),
        Prompt::NotPrivate { origin } => (ph::WARNING, vec![
            ("This form is on".into(), body(t.text_secondary(), Weight::Regular)),
            (origin.clone(), body(t.text_primary(), Weight::Semibold)),
            ("over http: anyone on the network can read it".into(), body(t.text_secondary(), Weight::Regular)),
        ]),
        Prompt::FetchAgain => (ph::ARROW_CLOCKWISE, vec![("This page could not be restored and will be fetched again".into(), body(t.text_secondary(), Weight::Regular))]),
        Prompt::Stored { at_s } => (ph::CLOCK, vec![
            ("The version you saw".into(), body(t.text_secondary(), Weight::Regular)),
            (crate::clock::when_label(*at_s, crate::clock::now_s()), body(t.text_primary(), Weight::Semibold)),
            ("from the store, offline".into(), body(t.text_secondary(), Weight::Regular)),
        ]),
        Prompt::Play { origin } => (ph::PLAY, vec![
            (origin.clone(), body(t.text_primary(), Weight::Semibold)),
            ("wants to play sound".into(), body(t.text_secondary(), Weight::Regular)),
        ]),
    };
    icon(ctx, r.x() + em(16.0), r.y() + em(12.0), g, 20.0, t.text_secondary());
    inline(ctx, r.x() + em(46.0), r.y() + em(22.0), em(6.0), &runs);
    let answers = p.answers();
    let bs = sans(type_size::SM, Weight::Semibold, t.text_primary());
    let mut x = r.x() + r.w() - em(16.0);
    for (i, a) in answers.iter().enumerate().rev() {
        let w = width(a, &bs) + em(24.0);
        x -= w;
        let primary = i == answers.len() - 1;
        let ghost = i == 0 && answers.len() > 2;
        let (edge, fill, hover, fg) = if primary {
            (t.accent_fill(), t.accent_fill(), t.accent_fill_hover(), t.text_on_accent())
        } else if ghost {
            (t.bg_elevated(), t.bg_elevated(), t.state_hover(), t.text_secondary())
        } else {
            (t.border_control(), t.bg_surface(), t.state_hover(), t.text_primary())
        };
        rect(ctx, x, r.y() + em(6.0), w, em(32.0), edge, em(6.0));
        let st2 = st.clone();
        area(ctx, Rect::new(x + 1.0, r.y() + em(7.0), w - 2.0, em(30.0)), em(5.0), fill, hover, move || with(&st2, |s| {
            if !s.answer(i, now_ms()) {
                eprintln!("navigator-ui: a press within 500 ms of the prompt appearing doesn't count");
            }
        }));
        text(ctx, x + em(12.0), r.y() + em(22.0), a, type_size::SM, Weight::Semibold, fg);
        ctx.pop();
        x -= em(8.0);
    }
}

// -- the page area ---------------------------------------------------------------

/// The page slot: the placeholder Limen's slot replaces in step 2 (the
/// page's own surface, from its jailed renderer). Until then it shows
/// what the trail knows about the page and its stand-in links, so the
/// trail can be walked. Returns the link under the pointer.
fn page_slot(ctx: &mut Ctx, g: &Geo, p: &trail::Page, st: &Mutable<Shell>) -> Option<String> {
    let t = ctx.theme;
    let r = g.page;
    if let Some(live) = &st.get_cloned().live {
        // The page is the doc-viewer child's window, bound into this slot
        // once navigatord has opened it; until then (or when it failed)
        // the slot is withdrawn and the UI says what is happening.
        let shown = matches!(live, crate::shell::LivePage::Shown(_));
        let slot = ctx.push(Node::Rect { rect: r, fill: if shown { Color::rgba(1.0, 1.0, 1.0, 1.0) } else { t.bg_canvas() }, radius: 0.0 });
        if shown {
            ctx.slot(slot, PAGE_SLOT);
        }
        let col = em(640.0).min(r.w() - em(64.0));
        let x = r.x() + ((r.w() - col) / 2.0).max(em(32.0));
        let y = r.y() + em(48.0);
        let ns = sans(type_size::SM, Weight::Regular, t.text_secondary());
        match live {
            crate::shell::LivePage::Loading(u) => {
                text(ctx, x, y, &fit(&format!("Opening {}…", crate::trail::host(u)), &ns, col), type_size::SM, Weight::Regular, t.text_secondary());
            }
            crate::shell::LivePage::Failed { url, why } => {
                text(ctx, x, y, &fit(&format!("{} could not be opened", crate::trail::host(url)), &ns, col), type_size::LG, Weight::Semibold, t.text_primary());
                text(ctx, x, y + em(28.0), &fit(why, &ns, col), type_size::SM, Weight::Regular, t.text_secondary());
            }
            crate::shell::LivePage::File { url, name } => {
                text(ctx, x, y, &fit(name, &ns, col), type_size::LG, Weight::Semibold, t.text_primary());
                text(ctx, x, y + em(28.0), &fit(&format!("A file from {}, not a page. Save it from the row above.", crate::trail::host(url)), &ns, col), type_size::SM, Weight::Regular, t.text_secondary());
            }
            crate::shell::LivePage::Shown(_) => {}
        }
        ctx.pop();
        return None;
    }
    let slot = ctx.push(Node::Rect { rect: r, fill: t.bg_canvas(), radius: 0.0 });
    ctx.slot(slot, PAGE_SLOT);
    let col = em(640.0).min(r.w() - em(64.0));
    let x = r.x() + ((r.w() - col) / 2.0).max(em(32.0));
    let mut y = r.y() + em(36.0);
    let (_, links) = fake::lookup(&p.url);
    if let Some(q) = &p.search {
        text(ctx, x, y + em(14.0), &format!("Results for “{q}”"), type_size::XXL, Weight::Semibold, t.text_primary());
    } else {
        let hs = sans(type_size::XXL, Weight::Semibold, t.text_primary());
        text(ctx, x, y + em(14.0), &fit(&p.title, &hs, col), type_size::XXL, Weight::Semibold, t.text_primary());
    }
    y += em(40.0);
    let ms = TextStyle { family: font::MONO.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() };
    ctx.add(Node::Text { rect: Rect::new(x, y, 0.0, em(16.0)), content: fit(&p.url, &ms, col), style: ms.clone() });
    y += em(28.0);
    // What this area is.
    boxed(ctx, x, y, col, em(56.0), t.border_default(), t.bg_surface(), em(8.0));
    text(ctx, x + em(14.0), y + em(18.0), "Page slot", type_size::XS, Weight::Semibold, t.text_secondary());
    let note = "The page's own surface goes here, drawn by its jailed renderer (Limen, build step 2).";
    let ns = sans(type_size::XS, Weight::Regular, t.text_tertiary());
    text(ctx, x + em(14.0), y + em(38.0), &fit(note, &ns, col - em(28.0)), type_size::XS, Weight::Regular, t.text_tertiary());
    y += em(76.0);
    let mut hovered = None;
    if !links.is_empty() {
        text(ctx, x, y + em(6.0), "STAND-IN LINKS · click to follow, Ctrl-click to open beside", type_size::XXS, Weight::Semibold, t.text_tertiary());
        y += em(18.0);
        for l in links {
            if y + em(30.0) > r.y() + r.h() {
                break;
            }
            let st2 = st.clone();
            let l2 = l.clone();
            let id = area(ctx, Rect::new(x - em(8.0), y, col + em(16.0), em(30.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| s.activate(&l2, now_ms())));
            if ctx.is_hovered(id) {
                hovered = Some(l.url.clone());
            }
            if l.download.is_some() {
                icon(ctx, x, y + em(7.0), ph::DOWNLOAD, 16.0, t.accent_fg());
            } else {
                icon(ctx, x, y + em(7.0), ph::ARROW_RIGHT, 16.0, t.accent_fg());
            }
            let ls = sans(type_size::SM, Weight::Medium, t.accent_fg());
            text(ctx, x + em(24.0), y + em(15.0), &fit(&l.title, &ls, col - em(24.0)), type_size::SM, Weight::Medium, t.accent_fg());
            ctx.pop();
            y += em(30.0);
        }
    }
    ctx.pop();
    hovered
}

/// Start (§5.4): a page the UI draws itself, nothing fetched. Kept pages
/// and trails put away; no feeds, nothing from a server.
fn start(ctx: &mut Ctx, s: &Shell, g: &Geo, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let r = g.page;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_canvas(), 0.0);
    let x = g.where_.x();
    let mut y = r.y() + em(32.0);
    // Leave room for the suggestions when they are open.
    if let Some(d) = g.drop {
        y = d.y() + d.h() + em(28.0);
    }
    if s.store.trail().unrecorded {
        icon(ctx, x, y, ph::EYE_SLASH, 16.0, t.text_secondary());
        text(ctx, x + em(24.0), y + em(8.0), "Unrecorded: nothing in this trail is written down, and its cookies go when it closes.", type_size::SM, Weight::Regular, t.text_secondary());
        y += em(36.0);
    }
    if !s.store.kept.is_empty() {
        text(ctx, x, y + em(8.0), "Kept", type_size::SM, Weight::Semibold, t.text_secondary());
        y += em(26.0);
        let mut cx = x;
        for k in &s.store.kept {
            if cx + em(176.0) > r.x() + r.w() - em(16.0) {
                break;
            }
            let st2 = st.clone();
            let url = k.url.clone();
            rect(ctx, cx, y, em(176.0), em(64.0), t.border_default(), em(8.0));
            area(ctx, Rect::new(cx + 1.0, y + 1.0, em(174.0), em(62.0)), em(7.0), t.bg_elevated(), t.state_hover(), move || with(&st2, |s| {
                s.end_edit();
                s.store.follow(fake::spec(&url));
            }));
            monogram(ctx, cx + em(14.0), y + em(14.0), &k.url, false);
            let ts = sans(type_size::SM, Weight::Medium, t.text_primary());
            text(ctx, cx + em(44.0), y + em(24.0), &fit(&k.title, &ts, em(100.0)), type_size::SM, Weight::Medium, t.text_primary());
            let ds = sans(type_size::XS, Weight::Regular, t.text_tertiary());
            text(ctx, cx + em(44.0), y + em(44.0), &fit(trail::host(&k.url), &ds, em(120.0)), type_size::XS, Weight::Regular, t.text_tertiary());
            icon(ctx, cx + em(152.0), y + em(8.0), ph::STAR, 12.0, t.text_tertiary());
            ctx.pop();
            cx += em(188.0);
        }
        y += em(96.0);
    }
    if !s.store.earlier.is_empty() {
        text(ctx, x, y + em(8.0), "Put away", type_size::SM, Weight::Semibold, t.text_secondary());
        y += em(26.0);
        for (i, e) in s.store.earlier.iter().enumerate() {
            if y + em(30.0) > r.y() + r.h() {
                break;
            }
            let st2 = st.clone();
            area(ctx, Rect::new(x - em(8.0), y, em(420.0), em(30.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st2, |s| {
                s.end_edit();
                // Bring it back in place of this empty trail on Start.
                s.store.put_away(crate::clock::now_s());
                s.store.bring_back(i);
            }));
            monogram(ctx, x, y + em(5.0), e.trail.page().map_or("", |p| &p.url), false);
            text(ctx, x + em(30.0), y + em(15.0), e.trail.title(), type_size::SM, Weight::Regular, t.text_primary());
            text(ctx, x + em(320.0), y + em(15.0), &crate::clock::when_label(e.at_s, crate::clock::now_s()), type_size::XS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            // Forget: its records go now (§4.5).
            let st3 = st.clone();
            let fid = area(ctx, Rect::new(x + em(432.0), y + em(3.0), em(24.0), em(24.0)), em(6.0), Color::TRANSPARENT, t.state_hover(), move || with(&st3, |s| {
                s.store.forget(i);
            }));
            icon(ctx, x + em(436.0), y + em(7.0), ph::X, 16.0, t.text_tertiary());
            ctx.pop();
            ctx.tooltip(fid, "Forget this trail");
            y += em(30.0);
        }
    }
}

/// A red page is not shown: the UI draws this in its place, with Go back
/// first and no way through for a public site (§6.5, D10).
fn stop_page(ctx: &mut Ctx, g: &Geo, p: &trail::Page, st: &Mutable<Shell>) {
    let t = ctx.theme;
    let r = g.page;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_canvas(), 0.0);
    let col = em(520.0).min(r.w() - em(64.0));
    let x = r.x() + (r.w() - col) / 2.0;
    let mut y = r.y() + (r.h() * 0.22).max(em(40.0));
    icon(ctx, x, y, ph::PROHIBIT, 32.0, t.danger_fg());
    y += em(52.0);
    text(ctx, x, y, "This page was stopped", type_size::XL, Weight::Semibold, t.text_primary());
    y += em(30.0);
    let ms = TextStyle { family: font::MONO.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_secondary() };
    ctx.add(Node::Text { rect: Rect::new(x, y - em(9.0), 0.0, em(18.0)), content: trail::host(&p.url).into(), style: ms });
    y += em(22.0);
    for reason in &p.facts.reasons {
        y = wrapped(ctx, x, y, col, reason, t.text_secondary());
    }
    y = wrapped(ctx, x, y + em(4.0), col, "Nothing from it was shown, and nothing you type can reach it.", t.text_tertiary());
    y += em(16.0);
    let bs = sans(type_size::SM, Weight::Semibold, t.text_on_accent());
    let w = width("Go back", &bs) + em(24.0);
    let st2 = st.clone();
    rect(ctx, x, y, w, em(32.0), t.accent_fill(), em(6.0));
    area(ctx, Rect::new(x + 1.0, y + 1.0, w - 2.0, em(30.0)), em(5.0), t.accent_fill(), t.accent_fill_hover(), move || with(&st2, |s| s.leave_stopped()));
    text(ctx, x + em(12.0), y + em(16.0), "Go back", type_size::SM, Weight::Semibold, t.text_on_accent());
    ctx.pop();
}

// -- the status bar --------------------------------------------------------------

/// The optional status bar (§5.7): where the link under the pointer goes,
/// as the broker resolved it, with the light it predicts. No network on
/// hover: the facts are the ones the store already holds.
fn status_bar(ctx: &mut Ctx, s: &Shell, r: Rect, link: Option<String>) {
    let t = ctx.theme;
    rect(ctx, r.x(), r.y(), r.w(), r.h(), t.bg_surface(), 0.0);
    rect(ctx, r.x(), r.y(), r.w(), 1.0, t.border_default(), 0.0);
    let Some(url) = link else { return };
    let st = |c: Color, w: Weight| sans(type_size::XS, w, c);
    let f = fake::spec(&url).facts;
    let cur_site = s.store.trail().page().map(|p| trail::site(&p.url).to_string()).unwrap_or_default();
    let site = trail::site(&url);
    icon(ctx, r.x() + em(12.0), r.y() + em(5.0), light_glyph(f.light), 14.0, light_color(ctx, f.light));
    let cy = r.y() + em(12.5);
    let scheme = if url.starts_with("http://") { "http://" } else { "https://" };
    let (before, domain, after) = trail::split_address(&url);
    inline(ctx, r.x() + em(32.0), cy, 0.0, &[
        (scheme.into(), st(t.text_tertiary(), Weight::Regular)),
        (before.into(), st(t.text_tertiary(), Weight::Regular)),
        (domain.into(), st(t.text_primary(), Weight::Semibold)),
        (after.into(), st(t.text_tertiary(), Weight::Regular)),
    ]);
    let (facts, c) = match f.light {
        Light::Red => (format!("{} · a click stops at a warning", f.reasons.first().map_or("unsafe", |s| s.trim_end_matches('.'))), t.danger_fg()),
        _ => {
            let rel = if site == cur_site { "same site" } else { "another site" };
            (format!("{rel} · {}", f.connection), t.text_secondary())
        }
    };
    let addr_w = width(&format!("{scheme}{before}{domain}{after}"), &st(t.text_primary(), Weight::Semibold));
    let fx = r.x() + em(32.0) + addr_w.max(em(260.0)) + em(24.0);
    let fs = st(c, Weight::Medium);
    text(ctx, fx, cy, &fit(&facts, &fs, r.x() + r.w() - fx - em(12.0)), type_size::XS, Weight::Medium, c);
}
