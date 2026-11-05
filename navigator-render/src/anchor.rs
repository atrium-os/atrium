//! ★ THE READER STAYS ON THE SAME ELEMENT ACROSS A ZOOM (backend §4.9).
//!
//! Zoom and text size re-lay the SAME document out, so the element under the
//! top of the viewport is still there; only its box moved and changed size.
//! The anchor is the same shape as progressive display's (an element and an
//! offset into it, `navigator-backend` `progressive::Anchor`), found and
//! mapped through `Geometry` instead of through the DOM: the offset is
//! carried as a FRACTION of the element's height, because after a reflow
//! the same line of text is that fraction of the way down, not the same
//! number of pixels.

use crate::html::Geometry;
use crate::U;

/// Where the reader is in one layout: an element (its geometry key) and how
/// far down it the viewport's top is, in 1/65536 of its border-box height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomAnchor {
    pub key: u64,
    pub frac: u32,
}

/// The element at `scroll_y`: the SMALLEST block-level or atomic box that
/// contains it (the most specific — a paragraph, not the article around
/// it), ties going to the later element in document order. `None` when no
/// laid-out box contains the position (past the end, or an empty page).
pub fn anchor_at(geo: &Geometry, scroll_y: U) -> Option<ZoomAnchor> {
    let mut best: Option<(U, u64)> = None;
    for (&k, g) in &geo.elements {
        if g.inline || g.h <= 0 || scroll_y < g.y || scroll_y >= g.y + g.h { continue }
        if best.is_none_or(|(h, bk)| g.h < h || (g.h == h && k > bk)) { best = Some((g.h, k)) }
    }
    let (h, key) = best?;
    let g = &geo.elements[&key];
    let frac = ((scroll_y - g.y) as i128 * 65536 / h as i128) as u32;
    Some(ZoomAnchor { key, frac })
}

/// The scroll position that puts the anchor's element at the same fraction
/// in another layout of the same document, clamped to the document. `None`
/// when the element was not laid out there (a media query hid it): the
/// caller keeps the reader at the same fraction of the document instead.
pub fn scroll_for(geo: &Geometry, a: ZoomAnchor, viewport_h: U) -> Option<U> {
    let g = geo.elements.get(&a.key)?;
    let y = g.y + (g.h as i128 * a.frac as i128 / 65536) as U;
    Some(y.clamp(0, (geo.height - viewport_h).max(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fontset::FontSet, html::layout_geometry, Subresources};
    use navigator_style::cascade::Env;

    #[test]
    fn the_reader_stays_on_the_same_paragraph_across_text_size_and_page_zoom() {
        let mut doc = String::from("<html><body>");
        for i in 0..40 { doc.push_str(&format!("<p k=\"{i}\">Paragraph {i}: {}</p>", "words that wrap across lines ".repeat(8))) }
        doc.push_str("</body></html>");
        let fonts = FontSet::load().unwrap();
        let subs = Subresources::new();
        let g100 = layout_geometry(&doc, &fonts, &Env::default(), &subs, "k");
        let p20 = &g100.elements[&20];
        // A third of the way into paragraph 20.
        let y = p20.y + p20.h / 3;
        let a = anchor_at(&g100, y).unwrap();
        assert_eq!(a.key, 20);
        for env in [Env { text_scale_64: 128, ..Env::default() }, Env::zoomed(800.0, 600.0, 192), Env { text_scale_64: 40, ..Env::default() }] {
            let g = layout_geometry(&doc, &fonts, &env, &subs, "k");
            let sy = scroll_for(&g, a, 600 * 64).unwrap();
            assert_ne!(g.elements[&20].y, p20.y, "the paragraph moved");
            let back = anchor_at(&g, sy).unwrap();
            assert_eq!(back.key, 20, "still on paragraph 20 at {env:?}");
            // The same fraction, to within one NSG unit (1/64 px) of position.
            let h = g.elements[&20].h as u64;
            assert!(back.frac.abs_diff(a.frac) as u64 * h / 65536 <= 1, "at the same fraction: {} vs {}", back.frac, a.frac);
        }
        assert_eq!(scroll_for(&g100, ZoomAnchor { key: 999, frac: 0 }, 600 * 64), None, "an element not laid out has no position");
    }
}
