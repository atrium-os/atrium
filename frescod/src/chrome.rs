//! Trusted chrome (F4d, docs/spec/fresco-f4-split.md; threat model §2.5.2):
//! a scene frescod builds itself, from its own state, composed above every
//! window. No client and no renderer contributes to it, so an app cannot
//! draw over it or fake it.
//!
//! Two parts, both for the focused window only:
//!
//! - **the strip**: the top [`STRIP_H`] pixels of the screen, opaque and
//!   always the top layer, so no app pixel ever shows there. It names the
//!   focused window's origin, when the Navigator set one over the chrome
//!   channel. An app can paint a look-alike anywhere in its own window, but
//!   never in the strip, so "what the strip says" is the one thing a user
//!   can rely on (the browser's line of death, held by the compositor).
//! - **a focus ring** just inside the focused window's edge, so the strip's
//!   claim is tied to a window on screen.
//!
//! Under each panel an app hangs from the bar by the bar's grant
//! (forum-status-items.md §4) it draws a tab naming the app, from the
//! launch registry: the app draws the panel, but not who it says it is.
//!
//! It also frames each credential slot that is fit to fill right now
//! (arca-autofill.md §5): a ring and a key tab no app or Arca draws, so a
//! look-alike field drawn by an app has none.
//!
//! Clicks on chrome pixels go to the WM, never to a window under them.
//!
//! Text is IBM Plex Mono, rasterized at build time (build.rs) for exactly
//! the characters an origin may hold (`fresco_split::chrome::origin_char`):
//! frescod parses no font file, and a look-alike letter has no glyph.

use fresco_scene_server::window::CHROME_LAYER;
use fresco_vulkan::SceneNode;

use crate::pack::WindowFrame;

/// The chrome layer's id: reserved, never a window's.
pub const CHROME_ID: u32 = CHROME_LAYER as u32;

const PAD: i32 = 4;
const RING: i32 = 2;

const RING_COLOR: [f32; 4] = [0.15, 0.55, 1.0, 1.0];
const TEXT_COLOR: [f32; 4] = [0.95, 0.95, 0.95, 1.0];

/// The chrome's glyphs: IBM Plex Mono, rasterized by build.rs.
mod glyphs {
    include!(concat!(env!("OUT_DIR"), "/chrome_glyphs.rs"));
}

/// A character's glyph: left bearing, top above the baseline, size, coverage.
fn glyph(c: u8) -> Option<(i32, i32, u32, u32, &'static [u8])> {
    glyphs::GLYPHS.iter().find(|g| g.0 == c).map(|g| (g.1, g.2, g.3, g.4, g.5))
}

fn rect(x: i32, y: i32, w: i32, h: i32, color: [f32; 4]) -> SceneNode {
    SceneNode { position: [x as f32, y as f32], size: [w as f32, h as f32], color, radius: 0.0, _pad: [0.0; 3] }
}

const SLOT_COLOR: [f32; 4] = [0.95, 0.75, 0.2, 1.0];
const KEY_COLOR: [f32; 4] = [0.1, 0.1, 0.12, 1.0];
/// The key tab's width, left of the slot.
const TAB: i32 = 18;

/// A credential slot's frame: a ring just outside `[x0, y0, x1, y1]` and
/// a tab on its left with a key drawn in it. Returns its bottom edge.
fn slot_frame(out: &mut Vec<SceneNode>, [x0, y0, x1, y1]: [i32; 4]) -> i32 {
    let (w, h) = (x1 - x0, y1 - y0);
    out.push(rect(x0 - RING, y0 - RING, w + 2 * RING, RING, SLOT_COLOR));
    out.push(rect(x0 - RING, y1, w + 2 * RING, RING, SLOT_COLOR));
    out.push(rect(x1, y0, RING, h, SLOT_COLOR));
    let tx = x0 - RING - TAB;
    out.push(rect(tx, y0 - RING, TAB + RING, h + 2 * RING, SLOT_COLOR));
    // The key: a round bow, a shaft and two teeth, centred in the tab.
    let cy = y0 + h / 2;
    let mut bow = rect(tx + 3, cy - 4, 8, 8, KEY_COLOR);
    bow.radius = 4.0;
    out.push(bow);
    out.push(rect(tx + 10, cy - 1, 6, 2, KEY_COLOR));
    out.push(rect(tx + 13, cy + 1, 2, 3, KEY_COLOR));
    out.push(rect(tx + 15, cy + 1, 1, 2, KEY_COLOR));
    y1 + RING
}

/// `text` with its line's top at (`x`, `y`), at most `max_w` pixels wide:
/// each glyph's coverage as runs of rects of one (quantized) alpha.
/// Characters without a glyph are not drawn (origins are checked before
/// they get here, so there are none).
fn text(out: &mut Vec<SceneNode>, s: &str, x: i32, y: i32, max_w: i32) {
    let base = y + glyphs::ASCENT;
    for (i, c) in s.bytes().enumerate() {
        let pen = x + i as i32 * glyphs::ADVANCE;
        if pen + glyphs::ADVANCE > x + max_w {
            break;
        }
        let Some((left, top, w, h, cov)) = glyph(c) else { continue };
        let level = |a: u8| (a as u32 + 8) / 17; // 0..=15
        for r in 0..h as usize {
            let row = &cov[r * w as usize..(r + 1) * w as usize];
            let mut col = 0;
            while col < row.len() {
                let l = level(row[col]);
                let start = col;
                while col < row.len() && level(row[col]) == l {
                    col += 1;
                }
                if l == 0 {
                    continue;
                }
                let mut c = TEXT_COLOR;
                c[3] = l as f32 / 15.0;
                out.push(rect(pen + left + start as i32, base - top + r as i32, (col - start) as i32, 1, c));
            }
        }
    }
}

/// Width of `s` in the chrome's font.
fn text_width(s: &str) -> i32 {
    s.len() as i32 * glyphs::ADVANCE
}

/// Height of the strip at the top of the screen that only frescod draws.
pub const STRIP_H: i32 = 24;

const STRIP_COLOR: [f32; 4] = [0.08, 0.08, 0.1, 1.0];

/// The chrome for this frame, on a `screen_w`-wide screen, given the
/// focused window and where each window is (`shown`: id and screen rect,
/// as composed). It is in screen coordinates (a layer at (0, 0)), down to
/// the bottom of the focused window's ring.
///
/// `covers`: the shell's own windows (the bar, its menus, notifications),
/// which stack above every app; the ring is left out where they are, so
/// it does not cut across an open bar menu. No app window is a cover.
///
/// `hosted`: the panels apps hang from the bar (screen rects), the app
/// each is, and whether it opened below its icon; a label in the gap
/// frescod leaves between the icon and the panel names it.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn build(screen_w: u32, focus: Option<u16>, origin: impl Fn(u16) -> Option<String>, shown: &[(u32, [i32; 4])], slots: &[[i32; 4]], covers: &[[i32; 4]], hosted: &[([i32; 4], String, bool)], islands: &[Island]) -> WindowFrame {
    let sw = screen_w as i32;
    let mut rects = vec![rect(0, 0, sw, STRIP_H, STRIP_COLOR)];
    let mut bottom = STRIP_H;
    for i in islands {
        bottom = bottom.max(island_frame(&mut rects, i, STRIP_H));
    }
    for (r, name, below) in hosted {
        bottom = bottom.max(hosted_label(&mut rects, *r, name, *below));
    }
    for &s in slots {
        bottom = bottom.max(slot_frame(&mut rects, s));
    }
    let focused = focus.filter(|&f| f != 0).and_then(|f| shown.iter().find(|(id, _)| *id == f as u32).map(|&(_, r)| (f, r)));
    if let Some((f, [x0, y0, x1, y1])) = focused {
        // The ring, clipped to the screen below the strip.
        let (x0, x1) = (x0.max(0), x1.min(sw));
        let y0 = y0.max(STRIP_H);
        let (w, h) = (x1 - x0, y1 - y0);
        if w > 2 * RING && h > 2 * RING {
            let sides = [
                [x0, y0, x1, y0 + RING],
                [x0, y1 - RING, x1, y1],
                [x0, y0 + RING, x0 + RING, y1 - RING],
                [x1 - RING, y0 + RING, x1, y1 - RING],
            ];
            for side in sides {
                for [a, b, c, d] in uncovered(side, covers) {
                    rects.push(rect(a, b, c - a, d - b, RING_COLOR));
                }
            }
            bottom = bottom.max(y1);
        }
        if let Some(o) = origin(f).filter(|o| !o.is_empty()) {
            rects.push(rect(PAD, 3, RING, STRIP_H - 6, RING_COLOR)); // ties the text to the ring
            text(&mut rects, &o, 3 * PAD, (STRIP_H - glyphs::LINE) / 2, sw - 4 * PAD);
        }
    }
    WindowFrame {
        id: CHROME_ID,
        pos: (0.0, 0.0),
        size: (screen_w, bottom.clamp(STRIP_H, i32::MAX) as u32),
        rects,
        ..Default::default()
    }
}

/// The label naming the app whose panel `[x0, y0, x1, y1]` hangs from
/// the bar: a pill in the strip's colours, with the ring's dot, in the gap
/// above the panel (`below`: it opened below its icon) or under it. It
/// covers none of the app's pixels. Returns its bottom edge.
fn hosted_label(out: &mut Vec<SceneNode>, [x0, y0, x1, y1]: [i32; 4], name: &str, below: bool) -> i32 {
    let h = LABEL_H;
    let gap = fresco_scene_server::window::HOSTED_LABEL_GAP as i32;
    let y = if below { y0 - gap + (gap - h) / 2 } else { y1 + (gap - h) / 2 };
    // The dot, a space, the name, the pill's round end.
    let tx = x0 + PAD + 1 + 6 + 6;
    let w = (tx - x0 + text_width(name) + 2 * PAD).min((x1 - x0).max(h));
    let mut pill = rect(x0, y, w, h, STRIP_COLOR);
    pill.radius = h as f32 / 2.0;
    out.push(pill);
    let mut dot = rect(x0 + PAD + 1, y + h / 2 - 3, 6, 6, RING_COLOR);
    dot.radius = 3.0;
    out.push(dot);
    text(out, name, tx, y + (h - glyphs::LINE) / 2, x0 + w - PAD - tx);
    y + h
}

/// The label's height.
const LABEL_H: i32 = 20;

/// An island slot as composed (navigator-ui.md §6.3): its screen rect,
/// whether anything above it overlaps it, and its origin if the window's
/// broker named one over the chrome channel.
#[derive(Clone, Debug, PartialEq)]
pub struct Island {
    pub rect: [i32; 4],
    pub covered: bool,
    pub origin: Option<String>,
}

impl Island {
    /// What its tag says, in the characters the chrome font draws.
    pub fn tag(&self) -> String {
        match self.origin.as_deref().filter(|o| !o.is_empty()) {
            Some(o) => format!("live code: {o}"),
            None => "live code".into(),
        }
    }
}

/// The island frame's colour: the tier indicator's amber (§6.3).
const ISLAND_COLOR: [f32; 4] = [0.96, 0.62, 0.12, 1.0];
/// A covered island's frame: dashes this long, gaps this long.
const DASH: i32 = 6;
const GAP: i32 = 4;

/// A 1 px frame just outside the island, dashed while it is covered,
/// and its tag in a pill above its top left corner (below it when there
/// is no room above `top`). Returns the bottom edge drawn.
fn island_frame(out: &mut Vec<SceneNode>, i: &Island, top: i32) -> i32 {
    let [x0, y0, x1, y1] = i.rect;
    let (fx0, fy0, fx1, fy1) = (x0 - 1, y0 - 1, x1 + 1, y1 + 1);
    let mut line = |a: i32, b: i32, horizontal: bool, at: i32| {
        if !i.covered {
            out.push(if horizontal { rect(a, at, b - a, 1, ISLAND_COLOR) } else { rect(at, a, 1, b - a, ISLAND_COLOR) });
            return;
        }
        let mut p = a;
        while p < b {
            let e = (p + DASH).min(b);
            out.push(if horizontal { rect(p, at, e - p, 1, ISLAND_COLOR) } else { rect(at, p, 1, e - p, ISLAND_COLOR) });
            p = e + GAP;
        }
    };
    line(fx0, fx1, true, fy0);
    line(fx0, fx1, true, fy1 - 1);
    line(fy0 + 1, fy1 - 1, false, fx0);
    line(fy0 + 1, fy1 - 1, false, fx1 - 1);
    let tag = i.tag();
    let w = text_width(&tag) + 2 * PAD + 6;
    let above = fy0 - LABEL_H;
    let y = if above >= top { above } else { fy1 };
    let mut pill = rect(fx0, y, w, LABEL_H, ISLAND_COLOR);
    pill.radius = 3.0;
    out.push(pill);
    text(out, &tag, fx0 + PAD + 3, y + (LABEL_H - glyphs::LINE) / 2, w - 2 * PAD);
    fy1.max(y + LABEL_H)
}

/// What the label over a granted panel names its app by: the launch
/// registry's id for `uid` (`uid-N` when it has none), in the characters
/// the chrome font draws. The registry is read again every few seconds.
pub fn app_label(uid: u32) -> String {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static REG: Mutex<Option<(Instant, portcullis_peer::AppRegistry)>> = Mutex::new(None);
    let mut g = REG.lock().unwrap();
    if g.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(5)) {
        let path = std::env::var("FRESCOD_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
        *g = Some((Instant::now(), portcullis_peer::AppRegistry::load(&path).unwrap_or_default()));
    }
    let id = g.as_ref().and_then(|(_, r)| r.resolve(uid)).map(|(_, app)| app.to_ascii_lowercase());
    match id {
        Some(a) => a.bytes().filter(|&c| fresco_split::chrome::origin_char(c)).map(char::from).collect(),
        None => format!("uid-{uid}"),
    }
}

/// The parts of `r` (`[x0, y0, x1, y1]`) outside every rect of `covers`.
fn uncovered(r: [i32; 4], covers: &[[i32; 4]]) -> Vec<[i32; 4]> {
    let mut parts = vec![r];
    for &[cx0, cy0, cx1, cy1] in covers {
        let mut next = Vec::new();
        for [x0, y0, x1, y1] in parts {
            if cx1 <= x0 || cx0 >= x1 || cy1 <= y0 || cy0 >= y1 {
                next.push([x0, y0, x1, y1]);
                continue;
            }
            // Above, below, then left and right of the cover, within r.
            if cy0 > y0 { next.push([x0, y0, x1, cy0]); }
            if cy1 < y1 { next.push([x0, cy1, x1, y1]); }
            let (my0, my1) = (y0.max(cy0), y1.min(cy1));
            if cx0 > x0 { next.push([x0, my0, cx0, my1]); }
            if cx1 < x1 { next.push([cx1, my0, x1, my1]); }
        }
        parts = next;
    }
    parts
}

/// Direct mode has no strip, so only the slots' frames go on top: a layer
/// at (0, 0) as wide and tall as the lowest, rightmost frame needs.
pub fn slot_frames(slots: &[[i32; 4]], islands: &[Island]) -> Option<WindowFrame> {
    if slots.is_empty() && islands.is_empty() {
        return None;
    }
    let mut rects = Vec::new();
    let (mut right, mut bottom) = (0, 0);
    for i in islands {
        bottom = bottom.max(island_frame(&mut rects, i, 0));
        right = right.max(i.rect[2] + 1).max(i.rect[0] + text_width(&i.tag()) + 4 * PAD);
    }
    for &s in slots {
        bottom = bottom.max(slot_frame(&mut rects, s));
        right = right.max(s[2] + RING);
    }
    Some(WindowFrame {
        id: CHROME_ID,
        pos: (0.0, 0.0),
        size: (right.max(1) as u32, bottom.max(1) as u32),
        rects,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_goes_under_the_shells_own_windows() {
        let shown = [(5u32, [0, 62, 1280, 670])];
        let lit = |f: &WindowFrame, x: i32, y: i32| {
            f.rects.iter().any(|n| n.color == RING_COLOR && (n.position[0] as i32..(n.position[0] + n.size[0]) as i32).contains(&x)
                && (n.position[1] as i32..(n.position[1] + n.size[1]) as i32).contains(&y))
        };
        let open = build(1280, Some(5), |_| None, &shown, &[], &[], &[], &[]);
        assert!(lit(&open, 800, 62) && lit(&open, 600, 62));
        // A bar menu over the window's top edge hides the ring under it only.
        let menu = [679, 56, 975, 92];
        let under = build(1280, Some(5), |_| None, &shown, &[], &[menu], &[], &[]);
        assert!(!lit(&under, 800, 62) && !lit(&under, 679, 63) && !lit(&under, 974, 62));
        assert!(lit(&under, 678, 62) && lit(&under, 975, 62) && lit(&under, 0, 300) && lit(&under, 640, 669));
        assert_eq!(uncovered([0, 0, 10, 10], &[[2, 2, 4, 4]]).iter().map(|[a, b, c, d]| (c - a) * (d - b)).sum::<i32>(), 96);
    }

    #[test]
    fn a_granted_panel_gets_a_label_naming_its_app() {
        let gap = fresco_scene_server::window::HOSTED_LABEL_GAP as i32;
        let f = build(1280, None, |_| None, &[], &[], &[], &[([900, 62 + gap, 1200, 362], "org.example.clip".into(), true)], &[]);
        let pill = f.rects.iter().find(|n| n.color == STRIP_COLOR && n.position[0] == 900.0 && n.radius > 0.0).expect("a label");
        let (top, bottom) = (pill.position[1] as i32, (pill.position[1] + pill.size[1]) as i32);
        assert!(top >= 62 && bottom <= 62 + gap, "in the gap above the panel: {top}..{bottom}");
        assert!(pill.size[0] < 300.0, "a pill, not the panel's width");
        let text: Vec<_> = f.rects.iter().filter(|n| n.color[..3] == TEXT_COLOR[..3]).collect();
        assert!(text.len() > 20, "the name is drawn");
        assert!(text.iter().all(|n| n.position[1] >= top as f32 && n.position[1] < bottom as f32), "inside the label");
        assert!(text.iter().any(|n| n.color[3] < 1.0), "anti-aliased");
        assert_eq!(app_label(4_000_000_000), "uid-4000000000");
    }

    #[test]
    fn slot_frames_cover_each_slot_and_its_tab() {
        assert!(super::slot_frames(&[], &[]).is_none());
        let f = super::slot_frames(&[[120, 320, 320, 360]], &[]).unwrap();
        assert!(f.size.0 >= 320 && f.size.1 >= 360);
        assert!(f.rects.iter().any(|r| r.position[0] < 120.0 - super::TAB as f32 + 1.0));
    }

    #[test]
    fn every_origin_character_has_a_glyph() {
        for c in 0u8..=127 {
            assert_eq!(super::glyph(c).is_some(), fresco_split::chrome::origin_char(c), "{:?}", c as char);
        }
        // Every glyph fits the strip's line.
        for g in glyphs::GLYPHS {
            assert_eq!(g.5.len(), (g.3 * g.4) as usize);
            assert!(glyphs::ASCENT - g.2 >= -1 && glyphs::ASCENT - g.2 + g.4 as i32 <= STRIP_H, "{:?}", g.0 as char);
        }
    }

    #[test]
    fn an_island_is_framed_and_tagged_and_dashed_while_covered() {
        let at = [200, 300, 600, 500];
        let clear = Island { rect: at, covered: false, origin: Some("media.example".into()) };
        assert_eq!(clear.tag(), "live code: media.example");
        assert_eq!(Island { origin: None, ..clear.clone() }.tag(), "live code");
        let sides = |f: &WindowFrame| f.rects.iter().filter(|n| n.color == ISLAND_COLOR && n.radius == 0.0).count();
        let f = build(1280, None, |_| None, &[], &[], &[], &[], &[clear.clone()]);
        assert_eq!(sides(&f), 4, "four solid sides");
        // The frame is just outside the island: none of its pixels.
        assert!(f.rects.iter().filter(|n| n.color == ISLAND_COLOR && n.radius == 0.0).all(|n| {
            let (x, y, w, h) = (n.position[0] as i32, n.position[1] as i32, n.size[0] as i32, n.size[1] as i32);
            x + w <= at[0] || x >= at[2] || y + h <= at[1] || y >= at[3]
        }));
        let pill = f.rects.iter().find(|n| n.color == ISLAND_COLOR && n.radius > 0.0).expect("a tag");
        assert_eq!(pill.position[1] as i32, at[1] - 1 - LABEL_H, "above the island");
        let covered = build(1280, None, |_| None, &[], &[], &[], &[], &[Island { covered: true, ..clear.clone() }]);
        assert!(sides(&covered) > 20, "dashed");
        // No room above, under the strip: the tag goes below.
        let high = build(1280, None, |_| None, &[], &[], &[], &[], &[Island { rect: [10, 30, 200, 100], ..clear.clone() }]);
        let pill = high.rects.iter().find(|n| n.color == ISLAND_COLOR && n.radius > 0.0).unwrap();
        assert_eq!(pill.position[1] as i32, 101);
        assert!(super::slot_frames(&[], &[clear]).is_some(), "direct mode frames islands too");
    }
}
