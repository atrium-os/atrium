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
//! It also frames each credential slot that is fit to fill right now
//! (arca-autofill.md §5): a ring and a key tab no app or Arca draws, so a
//! look-alike field drawn by an app has none.
//!
//! Clicks on chrome pixels go to the WM, never to a window under them.
//!
//! Text is drawn with a built-in 5x9 font of exactly the characters an
//! origin may hold (`fresco_split::chrome::origin_char`): frescod parses no
//! font file, and a look-alike letter has no glyph to be drawn with.

use fresco_scene_server::window::CHROME_LAYER;
use fresco_vulkan::SceneNode;

use crate::pack::WindowFrame;

/// The chrome layer's id: reserved, never a window's.
pub const CHROME_ID: u32 = CHROME_LAYER as u32;

/// Font cells per glyph and pixels per cell.
const GW: i32 = 5;
const GH: i32 = 9;
const SCALE: i32 = 2;
const ADVANCE: i32 = (GW + 1) * SCALE;
const PAD: i32 = 4;
const RING: i32 = 2;

const RING_COLOR: [f32; 4] = [0.15, 0.55, 1.0, 1.0];
const TEXT_COLOR: [f32; 4] = [0.95, 0.95, 0.95, 1.0];

/// Rows of each glyph, bit 4 the leftmost column.
const FONT: &[(u8, [u8; 9])] = &[
    (b'-', [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000]),
    (b'.', [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100, 0b00000, 0b00000]),
    (b'/', [0b00000, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b00000, 0b00000, 0b00000]),
    (b'0', [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'1', [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110, 0b00000, 0b00000]),
    (b'2', [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111, 0b00000, 0b00000]),
    (b'3', [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'4', [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010, 0b00000, 0b00000]),
    (b'5', [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'6', [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'7', [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00000, 0b00000]),
    (b'8', [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'9', [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100, 0b00000, 0b00000]),
    (b':', [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000, 0b00000, 0b00000]),
    (b'[', [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110, 0b00000, 0b00000]),
    (b']', [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110, 0b00000, 0b00000]),
    (b'_', [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000]),
    (b'a', [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111, 0b00000, 0b00000]),
    (b'b', [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110, 0b00000, 0b00000]),
    (b'c', [0b00000, 0b00000, 0b01110, 0b10000, 0b10000, 0b10000, 0b01110, 0b00000, 0b00000]),
    (b'd', [0b00001, 0b00001, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111, 0b00000, 0b00000]),
    (b'e', [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110, 0b00000, 0b00000]),
    (b'f', [0b00110, 0b01000, 0b11110, 0b01000, 0b01000, 0b01000, 0b01000, 0b00000, 0b00000]),
    (b'g', [0b00000, 0b00000, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110]),
    (b'h', [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b00000, 0b00000]),
    (b'i', [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110, 0b00000, 0b00000]),
    (b'j', [0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100]),
    (b'k', [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b00000, 0b00000]),
    (b'l', [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110, 0b00000, 0b00000]),
    (b'm', [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101, 0b00000, 0b00000]),
    (b'n', [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b00000, 0b00000]),
    (b'o', [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110, 0b00000, 0b00000]),
    (b'p', [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000]),
    (b'q', [0b00000, 0b00000, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001]),
    (b'r', [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000, 0b00000, 0b00000]),
    (b's', [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110, 0b00000, 0b00000]),
    (b't', [0b01000, 0b01000, 0b11110, 0b01000, 0b01000, 0b01001, 0b00110, 0b00000, 0b00000]),
    (b'u', [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101, 0b00000, 0b00000]),
    (b'v', [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100, 0b00000, 0b00000]),
    (b'w', [0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010, 0b00000, 0b00000]),
    (b'x', [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000, 0b00000]),
    (b'y', [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110]),
    (b'z', [0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111, 0b00000, 0b00000]),
];

fn glyph(c: u8) -> Option<&'static [u8; 9]> {
    FONT.iter().find(|(g, _)| *g == c).map(|(_, rows)| rows)
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

/// `text` at (`x`, `y`), one rect per run of lit cells in a row, at most
/// `max_w` pixels wide. Characters without a glyph are not drawn (origins
/// are checked before they get here, so there are none).
fn text(out: &mut Vec<SceneNode>, s: &str, x: i32, y: i32, max_w: i32) {
    for (i, c) in s.bytes().enumerate() {
        let gx = x + i as i32 * ADVANCE;
        if gx + GW * SCALE > x + max_w {
            break;
        }
        let Some(rows) = glyph(c) else { continue };
        for (r, bits) in rows.iter().enumerate() {
            let mut col = 0;
            while col < GW {
                if bits & (1 << (GW - 1 - col)) == 0 {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < GW && bits & (1 << (GW - 1 - col)) != 0 {
                    col += 1;
                }
                out.push(rect(gx + start * SCALE, y + r as i32 * SCALE, (col - start) * SCALE, SCALE, TEXT_COLOR));
            }
        }
    }
}

/// Height of the strip at the top of the screen that only frescod draws.
pub const STRIP_H: i32 = GH * SCALE + 2 * 3;

const STRIP_COLOR: [f32; 4] = [0.08, 0.08, 0.1, 1.0];

/// The chrome for this frame, on a `screen_w`-wide screen, given the
/// focused window and where each window is (`shown`: id and screen rect,
/// as composed). It is in screen coordinates (a layer at (0, 0)), down to
/// the bottom of the focused window's ring.
///
/// `covers`: the shell's own windows (the bar, its menus, notifications),
/// which stack above every app; the ring is left out where they are, so
/// it does not cut across an open bar menu. No app window is a cover.
pub fn build(screen_w: u32, focus: Option<u16>, origin: impl Fn(u16) -> Option<String>, shown: &[(u32, [i32; 4])], slots: &[[i32; 4]], covers: &[[i32; 4]]) -> WindowFrame {
    let sw = screen_w as i32;
    let mut rects = vec![rect(0, 0, sw, STRIP_H, STRIP_COLOR)];
    let mut bottom = STRIP_H;
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
            text(&mut rects, &o, 3 * PAD, 3, sw - 4 * PAD);
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
pub fn slot_frames(slots: &[[i32; 4]]) -> Option<WindowFrame> {
    if slots.is_empty() {
        return None;
    }
    let mut rects = Vec::new();
    let (mut right, mut bottom) = (0, 0);
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
        let open = build(1280, Some(5), |_| None, &shown, &[], &[]);
        assert!(lit(&open, 800, 62) && lit(&open, 600, 62));
        // A bar menu over the window's top edge hides the ring under it only.
        let menu = [679, 56, 975, 92];
        let under = build(1280, Some(5), |_| None, &shown, &[], &[menu]);
        assert!(!lit(&under, 800, 62) && !lit(&under, 679, 63) && !lit(&under, 974, 62));
        assert!(lit(&under, 678, 62) && lit(&under, 975, 62) && lit(&under, 0, 300) && lit(&under, 640, 669));
        assert_eq!(uncovered([0, 0, 10, 10], &[[2, 2, 4, 4]]).iter().map(|[a, b, c, d]| (c - a) * (d - b)).sum::<i32>(), 96);
    }

    #[test]
    fn slot_frames_cover_each_slot_and_its_tab() {
        assert!(super::slot_frames(&[]).is_none());
        let f = super::slot_frames(&[[120, 320, 320, 360]]).unwrap();
        assert!(f.size.0 >= 320 && f.size.1 >= 360);
        assert!(f.rects.iter().any(|r| r.position[0] < 120.0 - super::TAB as f32 + 1.0));
    }

    #[test]
    fn every_origin_character_has_a_glyph() {
        for c in 0u8..=127 {
            assert_eq!(super::glyph(c).is_some(), fresco_split::chrome::origin_char(c), "{:?}", c as char);
        }
    }
}
