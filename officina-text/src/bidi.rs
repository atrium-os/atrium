//! Right-to-left text (UAX #9).
//!
//! Glyphs stay in logical order everywhere a layout is measured or broken
//! into lines (the shaper's right-to-left output is turned back, see
//! `PinnedFonts::shape`). Each line then gets a visual order, which painting
//! walks, and which the caret and hit testing read through
//! [`ParaLayout::caret_x`](crate::layout::ParaLayout::caret_x) and
//! [`ParaLayout::hit_x`](crate::layout::ParaLayout::hit_x).

use crate::layout::Glyph;

/// The embedding level of every byte of `text`, and whether its base
/// direction is right to left (from its first strong character); `None`
/// when nothing in it is right to left, so the visual order is the logical
/// one.
pub fn levels(text: &str) -> Option<(Vec<u8>, bool)> {
    use unicode_bidi::BidiClass::{AL, AN, R, RLE, RLI, RLO};
    if !text.chars().any(|c| matches!(unicode_bidi::bidi_class(c), R | AL | AN | RLE | RLO | RLI)) {
        return None;
    }
    let info = unicode_bidi::BidiInfo::new(text, None);
    let rtl = info.paragraphs.first().is_some_and(|p| p.level.is_rtl());
    Some((info.levels.iter().map(|l| l.number()).collect(), rtl))
}

/// The visual order of `glyphs` (one line, logical order, clusters into
/// `text`), as indices from `first`: rule L1 puts the line's trailing
/// white space and its tabs at the base level, rule L2 reverses each run at
/// or above every odd level from the highest down. Empty when the line is
/// all left to right.
pub fn visual(glyphs: &[Glyph], text: &str, levels: &[u8], rtl: bool, first: usize) -> Vec<u32> {
    let base = rtl as u8;
    let bytes = text.as_bytes();
    let ws = |g: &Glyph| matches!(bytes.get(g.cluster as usize), Some(b' ' | b'\t'));
    let mut lv: Vec<u8> = glyphs.iter().map(|g| levels.get(g.cluster as usize).copied().unwrap_or(base)).collect();
    for (k, g) in glyphs.iter().enumerate().rev() {
        if !ws(g) {
            break;
        }
        lv[k] = base;
    }
    for (k, g) in glyphs.iter().enumerate() {
        if bytes.get(g.cluster as usize) == Some(&b'\t') {
            lv[k] = base;
        }
    }
    let Some(&max) = lv.iter().max() else { return Vec::new() };
    if max == 0 {
        return Vec::new();
    }
    let lowest_odd = lv.iter().copied().min().unwrap_or(0) | 1;
    let mut order: Vec<u32> = (first as u32..(first + glyphs.len()) as u32).collect();
    for level in (lowest_odd..=max).rev() {
        let mut i = 0;
        while i < lv.len() {
            if lv[i] < level {
                i += 1;
                continue;
            }
            let start = i;
            while i < lv.len() && lv[i] >= level {
                i += 1;
            }
            order[start..i].reverse();
            lv[start..i].reverse();
        }
    }
    order
}

/// `text` with each bracket that sits at a right-to-left level replaced by
/// its mirror (rule L4), for shaping: every pair is the same length in
/// UTF-8, so byte offsets still name the same characters.
pub fn mirrored(text: &str, levels: &[u8]) -> String {
    const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('«', '»'), ('‹', '›'), ('⁅', '⁆'), ('⁽', '⁾'), ('₍', '₎'), ('≤', '≥'), ('⟨', '⟩'), ('⟦', '⟧'), ('⦃', '⦄')];
    text.char_indices()
        .map(|(i, c)| {
            if levels.get(i).is_some_and(|l| l % 2 == 1) {
                if let Some(&(a, b)) = PAIRS.iter().find(|&&(a, b)| a == c || b == c) {
                    return if c == a { b } else { a };
                }
            }
            c
        })
        .collect()
}

/// `glyphs` (logical order, one line, clusters into `text`) in visual
/// order: for text drawn on one line outside a laid-out paragraph.
pub fn reorder(glyphs: Vec<Glyph>, text: &str) -> Vec<Glyph> {
    let Some((levels, rtl)) = levels(text) else { return glyphs };
    let order = visual(&glyphs, text, &levels, rtl, 0);
    if order.is_empty() {
        return glyphs;
    }
    order.iter().map(|&i| glyphs[i as usize]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(cluster: u32) -> Glyph {
        Glyph { face: 0, id: 0, adv: 1, dx: 0, dy: 0, cluster, fmt: 0 }
    }

    #[test]
    fn visual_order_reverses_right_to_left_runs() {
        assert!(levels("plain").is_none());
        // "ab אב": the Hebrew pair reversed after the Latin.
        let t = "ab אב";
        let (lv, rtl) = levels(t).unwrap();
        assert!(!rtl);
        let gl: Vec<Glyph> = t.char_indices().map(|(i, _)| g(i as u32)).collect();
        assert_eq!(visual(&gl, t, &lv, rtl, 0), vec![0, 1, 2, 4, 3]);
        // "אב cd": right to left base, the Latin word keeps its order and
        // goes left.
        let t = "אב cd";
        let (lv, rtl) = levels(t).unwrap();
        assert!(rtl);
        let gl: Vec<Glyph> = t.char_indices().map(|(i, _)| g(i as u32)).collect();
        assert_eq!(visual(&gl, t, &lv, rtl, 0), vec![3, 4, 2, 1, 0]);
        // Brackets mirror where they sit right to left, not elsewhere.
        let t = "x [y] אב (ג)";
        let (lv, _) = levels(t).unwrap();
        assert_eq!(mirrored(t, &lv), "x [y] אב )ג(");
    }
}
