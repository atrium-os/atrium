//! `navigator_mockup`: the Navigator window of docs/spec/navigator-ui.md
//! (trail rail, toolbar, prompt row, page), drawn by Pergola on a live
//! frescod. A mockup, not the UI: the trails are made up, the page is a
//! DocumentView in this process (the real page is a jailed Limen slot), and
//! the 24 px strip at the top stands in for frescod's origin strip, which
//! frescod draws itself in split mode.
//!
//!   FRESCO_SOCKET=… navigator_mockup [main|dark|start]
//!
//! Runs `SHOWCASE_SECS` (default 40) then exits.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pergola::geom::{Axis, Rect};
use pergola::layout::{Align, FlexStyle};
use pergola::node::Node;
use pergola::theme::{font, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Glyph, ScrollState};
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};
use pergola_document::{Doc, DocumentView};

const W: f32 = 1280.0;
const H: f32 = 720.0;
const STRIP: f32 = 24.0;
const RAIL: f32 = 248.0;
const RAIL_NARROW: f32 = 56.0;
const BAR: f32 = 40.0;
const PROMPT: f32 = 44.0;

// Phosphor codepoints (fonts/phosphor/Phosphor.ttf, by ligature name).
mod ph {
    pub const ARROW_LEFT: char = '\u{e058}';
    pub const ARROW_RIGHT: char = '\u{e06c}';
    pub const ARROW_CLOCKWISE: char = '\u{e036}';
    pub const MAGNIFYING_GLASS: char = '\u{e30c}';
    pub const TEXT_AA: char = '\u{e6ee}';
    pub const DOTS_THREE: char = '\u{e1fe}';
    pub const PLUS: char = '\u{e3d4}';
    pub const STAR: char = '\u{e46a}';
    pub const USER: char = '\u{e4c2}';
    pub const USER_CIRCLE: char = '\u{e4c4}';
    pub const CARET_DOWN: char = '\u{e136}';
    pub const SPEAKER: char = '\u{e450}';
    pub const SIDEBAR: char = '\u{ec24}';
    pub const DOWNLOAD: char = '\u{e20c}';
    pub const PLAY: char = '\u{e3d0}';
    pub const CLOCK: char = '\u{e1a0}';
    pub const PUSH_PIN: char = '\u{e3e2}';
    pub const BOOKMARK: char = '\u{e0ea}';
    pub const EYE_SLASH: char = '\u{e224}';
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Main,
    Dark,
    Start,
}

struct Mock {
    mode: Mode,
    doc: Option<DocumentView>,
}

fn style(t: &Semantic, size: f32, weight: Weight, color: Color) -> TextStyle {
    let _ = t;
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose cap band is centred on `cy` (pergola::text::wire_top).
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let t = ctx.theme;
    let h = size * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: style(&t, size, weight, color) });
}

fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) {
    ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius });
}

/// A 1 px edge round a box: an edge-coloured rect under a fill inset by 1.
fn boxed(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, edge: Color, fill: Color, radius: f32) {
    rect(ctx, x, y, w, h, edge, radius);
    rect(ctx, x + 1.0, y + 1.0, w - 2.0, h - 2.0, fill, (radius - 1.0).max(0.0));
}

fn icon(ctx: &mut Ctx, x: f32, y: f32, g: char, size: f32, color: Color) {
    Glyph::new(g).size(size).color(color).at(x, y).emit(ctx);
}

/// Texts in one row on a shared baseline, laid out by Pergola, centred
/// on `cy` (the first run's size sets the line).
fn inline(ctx: &mut Ctx, x: f32, cy: f32, gap: f32, runs: &[(&str, TextStyle)]) {
    let y = cy - 0.5 * runs.first().map(|r| r.1.size).unwrap_or(13.0);
    let r = ctx.push(Node::stack(Axis::Horizontal, Rect::new(x, y, 0.0, 0.0), gap));
    ctx.set_flex(r, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
    for (s, st) in runs {
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: (*s).into(), style: st.clone() });
    }
    ctx.pop();
}

/// The registrable domain's monogram: a neutral tile and one letter. No
/// favicons (navigator-ui.md §1, D6).
fn monogram(ctx: &mut Ctx, x: f32, y: f32, letter: &str, strong: bool) {
    let t = ctx.theme;
    rect(ctx, x, y, 20.0, 20.0, if strong { t.text_primary() } else { t.border_strong() }, 4.0);
    let letter = letter.to_uppercase();
    let letter = letter.as_str();
    let c = if strong { t.bg_canvas() } else { t.text_primary() };
    text(ctx, x + 6.0, y + 10.0, letter, type_size::XS, Weight::Semibold, c);
}

/// frescod's origin strip, as it looks in split mode (F4d): opaque,
/// above every app, origin only. Stand-in for the mockup.
fn strip(ctx: &mut Ctx, origin: &str, lane: Option<&str>) {
    rect(ctx, 0.0, 0.0, W, STRIP, Color::from_hex("#4E4F58"), 0.0);
    rect(ctx, 4.0, 4.0, 2.0, 16.0, Color::from_hex("#6CC4F5"), 0.0);
    let st = TextStyle { family: font::MONO.into(), size: 13.0, weight: Weight::Regular, color: Color::from_hex("#F2F4F6") };
    let lane_st = TextStyle { color: Color::from_hex("#E2B45C"), ..st.clone() };
    let mut runs = vec![(origin, st)];
    if let Some(l) = lane {
        runs.push((l, lane_st));
    }
    inline(ctx, 12.0, 12.0, 16.0, &runs);
}

struct Trail<'a> {
    letter: &'a str,
    title: &'a str,
    glyph: Option<char>,
}

fn rail(ctx: &mut Ctx, mode: Mode) {
    let t = ctx.theme;
    let top = STRIP;
    rect(ctx, 0.0, top, RAIL, H - top, t.bg_surface(), 0.0);
    rect(ctx, RAIL, top, 1.0, H - top, t.border_default(), 0.0);

    // Persona and New trail.
    boxed(ctx, 12.0, top + 10.0, 122.0, 28.0, t.border_control(), t.bg_elevated(), 6.0);
    icon(ctx, 20.0, top + 16.0, ph::USER_CIRCLE, 16.0, t.text_secondary());
    text(ctx, 42.0, top + 24.0, "Personal", type_size::SM, Weight::Medium, t.text_primary());
    icon(ctx, 112.0, top + 17.0, ph::CARET_DOWN, 12.0, t.text_secondary());
    icon(ctx, 160.0, top + 16.0, ph::PLUS, 16.0, t.text_primary());
    text(ctx, 180.0, top + 24.0, "Trail", type_size::SM, Weight::Medium, t.text_primary());
    icon(ctx, 222.0, top + 16.0, ph::SIDEBAR, 16.0, t.text_secondary());

    let label = |ctx: &mut Ctx, y: f32, s: &str| {
        let t = ctx.theme;
        text(ctx, 16.0, y + 6.0, s, type_size::XXS, Weight::Semibold, t.text_tertiary());
    };

    let mut y = top + 54.0;
    label(ctx, y, "TODAY");
    y += 18.0;

    // The current trail, expanded into its path.
    rect(ctx, 8.0, y, RAIL - 16.0, 32.0, t.bg_elevated(), 6.0);
    rect(ctx, 8.0, y + 8.0, 2.0, 16.0, t.accent_mark(), 1.0);
    monogram(ctx, 18.0, y + 6.0, "a", true);
    text(ctx, 46.0, y + 16.0, "Atrium (architecture)", type_size::SM, Weight::Semibold, t.text_primary());
    if mode == Mode::Dark {
        icon(ctx, 218.0, y + 8.0, ph::USER, 16.0, t.accent_fg());
    }
    y += 34.0;
    let path_top = y;
    let rows: [(&str, f32, bool, bool, Option<char>); 5] = [
        ("Search: roman atrium", 0.0, false, false, Some(ph::MAGNIFYING_GLASS)),
        ("Domus (house)", 0.0, false, false, None),
        ("Atrium (architecture)", 0.0, true, false, None),
        ("Impluvium", 14.0, false, true, None),
        ("Compluvium roofs", 14.0, false, false, None),
    ];
    for (s, indent, current, unseen, g) in rows {
        let x = 46.0 + indent;
        rect(ctx, x - 12.0 + 2.0, y + 13.0, 8.0, 1.0, t.border_strong(), 0.0);
        if let Some(g) = g {
            icon(ctx, x, y + 6.0, g, 14.0, t.text_tertiary());
            text(ctx, x + 18.0, y + 13.0, s, type_size::XS, Weight::Regular, t.text_secondary());
        } else {
            let (w, c) = if current { (Weight::Semibold, t.text_primary()) } else { (Weight::Regular, t.text_secondary()) };
            text(ctx, x, y + 13.0, s, type_size::XS, w, c);
        }
        if current {
            rect(ctx, 222.0, y + 10.0, 6.0, 6.0, t.accent_mark(), 3.0);
        }
        if unseen {
            rect(ctx, 222.0, y + 10.0, 6.0, 6.0, t.text_tertiary(), 3.0);
        }
        y += 26.0;
    }
    rect(ctx, 36.0, path_top + 2.0, 1.0, y - path_top - 15.0, t.border_strong(), 0.0);
    rect(ctx, 50.0, path_top + 3.0 * 26.0 - 6.0, 1.0, 2.0 * 26.0 - 5.0, t.border_strong(), 0.0);
    text(ctx, 60.0, y + 11.0, "1 more branch", type_size::XXS, Weight::Medium, t.text_tertiary());
    y += 26.0;

    let trails = [
        Trail { letter: "r", title: "Rust 1.92 release notes", glyph: None },
        Trail { letter: "f", title: "Handbook: Jails", glyph: None },
        Trail { letter: "s", title: "Late-night radio", glyph: Some(ph::SPEAKER) },
        Trail { letter: "m", title: "Weather, Chennai", glyph: None },
    ];
    for tr in trails {
        monogram(ctx, 18.0, y + 6.0, tr.letter, false);
        text(ctx, 46.0, y + 16.0, tr.title, type_size::SM, Weight::Regular, t.text_primary());
        if let Some(g) = tr.glyph {
            icon(ctx, 218.0, y + 8.0, g, 16.0, t.text_secondary());
        }
        y += 32.0;
    }

    y += 10.0;
    label(ctx, y, "EARLIER");
    y += 18.0;
    for (l, s, d) in [("b", "Bread hydration tables", "Wed"), ("p", "Pantheon dome concrete", "Wed"), ("t", "Tessera CAS notes", "Mon")] {
        monogram(ctx, 18.0, y + 6.0, l, false);
        text(ctx, 46.0, y + 16.0, s, type_size::SM, Weight::Regular, t.text_secondary());
        text(ctx, 206.0, y + 16.0, d, type_size::XXS, Weight::Regular, t.text_tertiary());
        y += 32.0;
    }
}

/// The rail collapsed to monograms (Ctrl-B), the dock rail's width.
fn rail_narrow(ctx: &mut Ctx) {
    let t = ctx.theme;
    let top = STRIP;
    rect(ctx, 0.0, top, RAIL_NARROW, H - top, t.bg_surface(), 0.0);
    rect(ctx, RAIL_NARROW, top, 1.0, H - top, t.border_default(), 0.0);
    icon(ctx, 20.0, top + 14.0, ph::SIDEBAR, 16.0, t.text_secondary());
    let mut y = top + 50.0;
    rect(ctx, 8.0, y, 40.0, 32.0, t.bg_elevated(), 6.0);
    rect(ctx, 8.0, y + 8.0, 2.0, 16.0, t.accent_mark(), 1.0);
    icon(ctx, 20.0, y + 8.0, ph::PLUS, 16.0, t.text_primary());
    y += 40.0;
    for l in ["a", "r", "f", "s", "m"] {
        monogram(ctx, 18.0, y + 6.0, l, false);
        y += 36.0;
    }
}

fn toolbar(ctx: &mut Ctx, x0: f32, mode: Mode) {
    let t = ctx.theme;
    let top = STRIP;
    rect(ctx, x0 + 1.0, top, W - x0 - 1.0, BAR, t.bg_surface(), 0.0);
    rect(ctx, x0 + 1.0, top + BAR, W - x0 - 1.0, 1.0, t.border_default(), 0.0);
    let start = mode == Mode::Start;
    let dim = t.text_disabled();
    icon(ctx, x0 + 14.0, top + 10.0, ph::ARROW_LEFT, 20.0, if start { dim } else { t.text_primary() });
    icon(ctx, x0 + 46.0, top + 10.0, ph::ARROW_RIGHT, 20.0, if start { dim } else { t.text_secondary() });
    icon(ctx, x0 + 78.0, top + 10.0, ph::ARROW_CLOCKWISE, 20.0, if start { dim } else { t.text_secondary() });

    // The Where field.
    let fx = x0 + 112.0;
    let fw = W - fx - 236.0;
    let body = |c: Color, w: Weight| TextStyle { family: font::SANS.into(), size: type_size::SM, weight: w, color: c };
    if start {
        boxed(ctx, fx - 2.0, top + 4.0, fw + 4.0, 32.0, t.focus_ring(), t.focus_ring(), 8.0);
        boxed(ctx, fx, top + 6.0, fw, 28.0, t.border_control(), t.bg_canvas(), 6.0);
        icon(ctx, fx + 10.0, top + 13.0, ph::MAGNIFYING_GLASS, 14.0, t.text_tertiary());
        inline(ctx, fx + 30.0, top + 20.0, 0.0, &[("impl", body(t.text_primary(), Weight::Regular))]);
        rect(ctx, fx + 57.0, top + 11.0, 1.0, 18.0, t.text_primary(), 0.0);
    } else {
        boxed(ctx, fx, top + 6.0, fw, 28.0, t.border_default(), t.bg_canvas(), 6.0);
        let y = top + 20.0 - type_size::SM * 0.5;
        let r = ctx.push(Node::stack(Axis::Horizontal, Rect::new(fx + 12.0, y, 0.0, 0.0), 8.0));
        ctx.set_flex(r, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "Atrium (architecture)".into(), style: body(t.text_primary(), Weight::Medium) });
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "·".into(), style: body(t.text_tertiary(), Weight::Regular) });
        let a = ctx.push(Node::stack(Axis::Horizontal, Rect::ZERO_SIZED, 0.0));
        ctx.set_flex(a, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "atlas.example".into(), style: body(t.text_primary(), Weight::Semibold) });
        ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "/wiki/Atrium_(architecture)".into(), style: body(t.text_tertiary(), Weight::Regular) });
        ctx.pop();
        ctx.pop();
    }

    // Presence, then Aa, Keep, More.
    let px = fx + fw + 12.0;
    if mode == Mode::Dark {
        boxed(ctx, px, top + 6.0, 112.0, 28.0, t.accent_mark(), t.accent_bg(), 14.0);
        icon(ctx, px + 9.0, top + 12.0, ph::USER, 16.0, t.accent_strong());
        text(ctx, px + 29.0, top + 20.0, "atlas.example", type_size::XS, Weight::Semibold, t.accent_strong());
    } else if !start {
        boxed(ctx, px, top + 6.0, 112.0, 28.0, t.border_default(), t.bg_surface(), 14.0);
        icon(ctx, px + 9.0, top + 12.0, ph::EYE_SLASH, 16.0, t.text_secondary());
        text(ctx, px + 29.0, top + 20.0, "Anonymous", type_size::XS, Weight::Medium, t.text_secondary());
    }
    icon(ctx, W - 72.0 - 28.0, top + 10.0, ph::TEXT_AA, 20.0, if start { dim } else { t.text_secondary() });
    icon(ctx, W - 72.0, top + 10.0, ph::STAR, 20.0, if start { dim } else { t.text_secondary() });
    icon(ctx, W - 40.0, top + 10.0, ph::DOTS_THREE, 20.0, t.text_secondary());
}

/// The prompt row: asked by the Navigator, between the toolbar and the page.
fn prompt(ctx: &mut Ctx, x0: f32) -> f32 {
    let t = ctx.theme;
    let y = STRIP + BAR + 1.0;
    rect(ctx, x0 + 1.0, y, W - x0 - 1.0, PROMPT, t.bg_elevated(), 0.0);
    rect(ctx, x0 + 1.0, y + PROMPT - 1.0, W - x0 - 1.0, 1.0, t.border_default(), 0.0);
    icon(ctx, x0 + 16.0, y + 12.0, ph::DOWNLOAD, 20.0, t.text_secondary());
    let body = |c: Color, w: Weight| TextStyle { family: font::SANS.into(), size: type_size::SM, weight: w, color: c };
    inline(ctx, x0 + 46.0, y + 22.0, 6.0, &[
        ("impluvium-survey.pdf", body(t.text_primary(), Weight::Semibold)),
        ("2.4 MB from", body(t.text_secondary(), Weight::Regular)),
        ("atlas.example", body(t.text_primary(), Weight::Semibold)),
    ]);
    // Buttons drawn to the controls sheet's look (visual language §14).
    let b = |ctx: &mut Ctx, x: f32, w: f32, label: &str, kind: u8| {
        let t = ctx.theme;
        let (edge, fill, fg) = match kind {
            0 => (t.bg_elevated(), t.bg_elevated(), t.text_secondary()),
            1 => (t.border_control(), t.bg_surface(), t.text_primary()),
            _ => (t.accent_fill(), t.accent_fill(), t.text_on_accent()),
        };
        boxed(ctx, x, y + 6.0, w, 32.0, edge, fill, 6.0);
        text(ctx, x + 12.0, y + 22.0, label, type_size::SM, Weight::Semibold, fg);
    };
    b(ctx, W - 300.0, 70.0, "Cancel", 0);
    b(ctx, W - 222.0, 58.0, "Save", 1);
    b(ctx, W - 156.0, 140.0, "Open in viewer", 2);
    y + PROMPT
}

/// An island: a separate slot above the page, framed and tagged by
/// frescod (navigator-ui.md §6.3). Stand-in drawn here.
fn island(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32) {
    let t = ctx.theme;
    let frame = Color::from_hex("#E2B45C");
    rect(ctx, x - 1.0, y - 1.0, w + 2.0, h + 2.0, frame, 2.0);
    rect(ctx, x, y, w, h, Color::from_hex("#0B0D10"), 0.0);
    // The film still: a few calm bands, then the controls.
    rect(ctx, x, y + h * 0.55, w, h * 0.45, Color::from_hex("#1C2229"), 0.0);
    rect(ctx, x + w * 0.18, y + h * 0.25, w * 0.64, h * 0.42, Color::from_hex("#3A3128"), 0.0);
    rect(ctx, x + w * 0.30, y + h * 0.18, w * 0.40, h * 0.10, Color::from_hex("#5A4A38"), 0.0);
    rect(ctx, x + w / 2.0 - 28.0, y + h / 2.0 - 28.0, 56.0, 56.0, Color::from_hex("#F2F4F6"), 28.0);
    icon(ctx, x + w / 2.0 - 11.0, y + h / 2.0 - 12.0, ph::PLAY, 24.0, Color::from_hex("#0E1114"));
    rect(ctx, x + 16.0, y + h - 18.0, w - 32.0, 3.0, Color::from_hex("#5A636C"), 1.5);
    rect(ctx, x + 16.0, y + h - 18.0, (w - 32.0) * 0.12, 3.0, Color::from_hex("#F2F4F6"), 1.5);
    // The tag.
    rect(ctx, x - 1.0, y - 21.0, 168.0, 20.0, frame, 0.0);
    text(ctx, x + 6.0, y - 11.0, "site code · media.example", type_size::XS, Weight::Semibold, Color::from_hex("#22282E"));
    let _ = t;
}

/// Start: a page the UI draws itself, nothing fetched (§5.4), with the
/// Where field's local suggestions open.
fn start_page(ctx: &mut Ctx, x0: f32) {
    let t = ctx.theme;
    let top = STRIP + BAR + 1.0;
    rect(ctx, x0 + 1.0, top, W - x0 - 1.0, H - top, t.bg_canvas(), 0.0);

    // The suggestions pop-up (a Fresco pop-up window in the real UI).
    let fx = x0 + 112.0;
    let fw = W - fx - 236.0;
    let py = top + 4.0;
    let ph_ = 232.0;
    boxed(ctx, fx, py, fw, ph_, t.border_default(), t.bg_elevated(), 8.0);
    let sec = |ctx: &mut Ctx, y: f32, s: &str| {
        let t = ctx.theme;
        text(ctx, fx + 14.0, y + 6.0, s, type_size::XXS, Weight::Semibold, t.text_tertiary());
    };
    let body = |c: Color, w: Weight| TextStyle { family: font::SANS.into(), size: type_size::SM, weight: w, color: c };
    let mut y = py + 10.0;
    sec(ctx, y, "OPEN TRAILS");
    y += 16.0;
    rect(ctx, fx + 6.0, y, fw - 12.0, 30.0, t.state_hover(), 6.0);
    monogram(ctx, fx + 14.0, y + 5.0, "a", true);
    inline(ctx, fx + 44.0, y + 15.0, 10.0, &[
        ("Impluvium", body(t.text_primary(), Weight::Medium)),
        ("in trail “Atrium (architecture)”", body(t.text_tertiary(), Weight::Regular)),
    ]);
    text(ctx, fx + fw - 92.0, y + 15.0, "Switch to", type_size::XS, Weight::Medium, t.accent_fg());
    y += 36.0;
    sec(ctx, y, "KEPT AND HISTORY");
    y += 16.0;
    for (g, a, b) in [
        (ph::BOOKMARK, "Implementing Iterator", "doc.rust.example/std/iter"),
        (ph::CLOCK, "Impluvium: water in the Roman house", "atlas.example/wiki/Impluvium"),
        (ph::CLOCK, "impl Trait in argument position", "blog.rust.example/2026/impl-trait"),
    ] {
        icon(ctx, fx + 16.0, y + 7.0, g, 16.0, t.text_tertiary());
        inline(ctx, fx + 44.0, y + 15.0, 10.0, &[(a, body(t.text_primary(), Weight::Regular)), (b, body(t.text_tertiary(), Weight::Regular))]);
        y += 30.0;
    }
    rect(ctx, fx + 1.0, y + 2.0, fw - 2.0, 1.0, t.border_default(), 0.0);
    y += 8.0;
    icon(ctx, fx + 16.0, y + 7.0, ph::MAGNIFYING_GLASS, 16.0, t.text_tertiary());
    inline(ctx, fx + 44.0, y + 15.0, 6.0, &[
        ("Search for “impl”", body(t.text_primary(), Weight::Regular)),
        ("· sent only when you press Enter", body(t.text_tertiary(), Weight::Regular)),
    ]);

    // Start's own content, below the pop-up.
    let cx = x0 + 112.0;
    let mut y = py + ph_ + 40.0;
    text(ctx, cx, y + 8.0, "Pinned", type_size::SM, Weight::Semibold, t.text_secondary());
    y += 26.0;
    let mut x = cx;
    for (l, s, d) in [("h", "Handbook", "freebsd.example"), ("r", "Rust std", "doc.rust.example"), ("w", "Weather", "met.example"), ("n", "Notes", "tabula (local)")] {
        boxed(ctx, x, y, 176.0, 64.0, t.border_default(), t.bg_elevated(), 8.0);
        monogram(ctx, x + 14.0, y + 14.0, l, false);
        text(ctx, x + 44.0, y + 24.0, s, type_size::SM, Weight::Medium, t.text_primary());
        text(ctx, x + 44.0, y + 44.0, d, type_size::XS, Weight::Regular, t.text_tertiary());
        icon(ctx, x + 152.0, y + 8.0, ph::PUSH_PIN, 12.0, t.text_tertiary());
        x += 188.0;
    }
    y += 96.0;
    text(ctx, cx, y + 8.0, "Put away today", type_size::SM, Weight::Semibold, t.text_secondary());
    y += 26.0;
    for (l, s, d) in [("b", "Bread hydration tables", "10:12"), ("p", "Pantheon dome concrete", "09:40"), ("t", "Tessera CAS notes", "08:55")] {
        monogram(ctx, cx, y + 4.0, l, false);
        text(ctx, cx + 30.0, y + 14.0, s, type_size::SM, Weight::Regular, t.text_primary());
        text(ctx, cx + 300.0, y + 14.0, d, type_size::XS, Weight::Regular, t.text_tertiary());
        y += 30.0;
    }
}

impl View for Mock {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, W, H), fill: t.bg_canvas(), radius: 0.0 });
        match self.mode {
            Mode::Start => {
                rail_narrow(ctx);
                toolbar(ctx, RAIL_NARROW, self.mode);
                start_page(ctx, RAIL_NARROW);
                strip(ctx, "new trail · nothing loaded", None);
            }
            Mode::Main | Mode::Dark => {
                if let Some(doc) = &self.doc {
                    doc.render(ctx);
                }
                rail(ctx, self.mode);
                toolbar(ctx, RAIL, self.mode);
                if self.mode == Mode::Dark {
                    prompt(ctx, RAIL);
                    island(ctx, RAIL + 1.0 + 140.0, 374.0, 520.0, 272.0);
                }
                strip(ctx, "https://atlas.example", None);
            }
        }
    }
}

fn page(island_gap: bool) -> String {
    let mut b = String::from(r#"<html><head><style>
        body { margin-top: 28px; margin-left: 140px; margin-right: 140px; font-size: 16px; line-height: 1.55 }
        h1 { font-size: 30px; margin: 0px 0px 6px; font-weight: 600 }
        .sub { color: #7c858e; font-size: 14px; margin: 0px 0px 18px }
        h2 { font-size: 20px; margin: 22px 0px 8px; font-weight: 600 }
        p { margin: 0px 0px 12px }
        .gap { height: 330px }
        @media (prefers-color-scheme: dark) { body { background-color: #12161a; color: #e4e8ec } .sub { color: #a8b0b8 } }
        </style></head><body>
        <h1>Atrium (architecture)</h1>
        <p class="sub">From the atlas of building types</p>"#);
    if island_gap {
        b.push_str(r#"<p>An atrium is the open central court of a Roman house, lit from a square opening in the roof. The short film below walks through one from the street door to the garden.</p><div class="gap"></div>"#);
    } else {
        b.push_str(r#"<p>An <b>atrium</b> is the open central court of a Roman house. Rooms open onto it from every side, and light falls into it through a square opening in the roof, the <i>compluvium</i>. Rain follows the light in and collects in a shallow basin set into the floor below, the <i>impluvium</i>, which feeds a cistern under the house.</p>
        <p>Visitors came in from the street through a narrow passage and met the household in the atrium first. It was the most public room of a private house: the place where clients waited in the morning, where the family's portraits were kept, and where the owner chose what a guest would see.</p>"#);
    }
    b.push_str(r#"<h2>Plan</h2>
        <p>The atrium sits on the axis that runs from the street door to the garden. On either side are small rooms, and at the far end the <i>tablinum</i>, the owner's room, opens fully onto it so that a visitor standing at the door could see straight through the house to the planted court behind.</p>
        <h2>Roofs</h2>
        <p>Writers on architecture named four kinds of atrium by how the roof was carried: on two beams with no columns, on four columns at the corners of the opening, on many columns, or with the roof sloping outward so that the rain fell away from the court. The last kind kept the house dry but gave up the cistern.</p>
        <h2>Later use</h2>
        <p>The name passed to the forecourts of early churches, and much later to the tall covered halls of hotels and offices, which keep the idea of a room lit from above and open to everything around it, and lose the water.</p>"#);
    b.push_str("</body></html>");
    b
}

fn main() -> std::io::Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("dark") => Mode::Dark,
        Some("start") => Mode::Start,
        _ => Mode::Main,
    };
    let dark = mode == Mode::Dark;
    let secs: u64 = std::env::var("SHOWCASE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut window = Window::open(&WindowDesc::new("Navigator", SizeSpec::Fixed(W as u32, H as u32)))?;
    window.follow_appearance(false);
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let doc = if mode == Mode::Start {
        None
    } else {
        let top = STRIP + BAR + 1.0 + if dark { PROMPT } else { 0.0 };
        let (w, h) = (W - RAIL - 1.0, H - top);
        let d = Rc::new(RefCell::new(Doc::html(page(dark), Default::default(), w, h, dark, |_| None).map_err(std::io::Error::other)?));
        Some(DocumentView::new(Mutable::with_dirty(ScrollState::default(), dirty.clone()), Rect::new(RAIL + 1.0, top, w, h), d, Mutable::with_dirty(0u64, dirty.clone())))
    };
    let view = Mock { mode, doc };
    let mut app = App::new_with_flag(view, dirty).with_theme(if dark { Semantic::DARK } else { Semantic::LIGHT });
    let end = Instant::now() + Duration::from_secs(secs);
    window.paint(&mut app)?;
    while Instant::now() < end {
        window.step(&mut app, Some(Duration::from_millis(200)))?;
    }
    Ok(())
}
