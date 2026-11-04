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
pub fn build(screen_w: u32, focus: Option<u16>, origin: impl Fn(u16) -> Option<String>, shown: &[(u32, [i32; 4])]) -> WindowFrame {
    let sw = screen_w as i32;
    let mut rects = vec![rect(0, 0, sw, STRIP_H, STRIP_COLOR)];
    let mut bottom = STRIP_H;
    let focused = focus.filter(|&f| f != 0).and_then(|f| shown.iter().find(|(id, _)| *id == f as u32).map(|&(_, r)| (f, r)));
    if let Some((f, [x0, y0, x1, y1])) = focused {
        // The ring, clipped to the screen below the strip.
        let (x0, x1) = (x0.max(0), x1.min(sw));
        let y0 = y0.max(STRIP_H);
        let (w, h) = (x1 - x0, y1 - y0);
        if w > 2 * RING && h > 2 * RING {
            rects.push(rect(x0, y0, w, RING, RING_COLOR));
            rects.push(rect(x0, y1 - RING, w, RING, RING_COLOR));
            rects.push(rect(x0, y0 + RING, RING, h - 2 * RING, RING_COLOR));
            rects.push(rect(x1 - RING, y0 + RING, RING, h - 2 * RING, RING_COLOR));
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

#[cfg(test)]
mod tests {
    #[test]
    fn every_origin_character_has_a_glyph() {
        for c in 0u8..=127 {
            assert_eq!(super::glyph(c).is_some(), fresco_split::chrome::origin_char(c), "{:?}", c as char);
        }
    }
}
