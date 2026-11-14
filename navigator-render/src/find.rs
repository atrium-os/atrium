//! Find in page (navigator-ui.md §7): where a query's matches are in a
//! laid-out document, as rects, never as text leaving the worker.
//!
//! The document is laid out whole, the same way its view lays it out
//! (same window, zoom, text scale and appearance), and each text run of
//! each band is searched, case folded. A match must lie inside one run:
//! one that a line break or a change of style splits is not found.

use crate::bands::Banded;
use crate::fontset::FontSet;
use crate::{Subresources, U};
use navigator_style::cascade::Env;

/// 1/64 px.
const PX: f64 = 64.0;

/// Each match of `query` in `html` laid out at `env`, in document order,
/// as `[x, y, w, h]` in CSS px from the document's top left; at most `max`.
pub fn find(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, query: &str, max: usize) -> Vec<[f32; 4]> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() || max == 0 {
        return vec![];
    }
    let mut b = Banded::new(html, fonts, env, subs, (env.height_px * PX) as U);
    let mut out = Vec::new();
    let mut k = 0;
    while out.len() < max {
        if !b.ensure(k) {
            if b.scene(k).is_none() {
                break;
            }
        }
        let Some(scene) = b.scene(k) else { break };
        let mut runs: Vec<&crate::Run> = scene.runs.iter().collect();
        runs.sort_by_key(|r| (r.y, r.x));
        for r in runs {
            matches_in(r, advance(fonts, r), &q, &mut out, max);
            if out.len() >= max {
                break;
            }
        }
        b.evict(k);
        k += 1;
    }
    out
}

/// The first word of the first line at or below `y` (CSS px from the
/// document's top) in `html` laid out at `env`, and that line's top, as
/// `find` places it: what a reader at `y` is looking at, to find again in
/// another layout of the same page (a new text size keeps the reader on
/// the same words, resolution-independence.md §10). Words of letters and
/// digits only, so `find` finds it whole.
pub fn word_at(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, y: f32) -> Option<(String, f32)> {
    let mut b = Banded::new(html, fonts, env, subs, (env.height_px * PX) as U);
    let mut k = 0;
    loop {
        if !b.ensure(k) && b.scene(k).is_none() {
            return None;
        }
        let scene = b.scene(k)?;
        let mut runs: Vec<&crate::Run> = scene.runs.iter().collect();
        runs.sort_by_key(|r| (r.y, r.x));
        for r in runs {
            let top = (r.y as f64 / PX - r.size as f64 / PX * 0.85) as f32;
            if top + 0.5 < y {
                continue;
            }
            if let Some(word) = r.text.split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()) {
                return Some((word.to_string(), top));
            }
        }
        b.evict(k);
        k += 1;
    }
}

/// The advance of a run's last glyph, in px: where the run ends past its
/// last glyph's start, so a match at the end covers all of that glyph.
fn advance(fonts: &FontSet, r: &crate::Run) -> f64 {
    let size = r.size as f64 / PX;
    let measured = (|| {
        let f = fonts.faces.get(r.face)?;
        let gid = r.glyphs.last()?.0;
        let a = ttf_parser::Face::parse(&f.bytes, 0).ok()?.glyph_hor_advance(ttf_parser::GlyphId(gid))?;
        Some(a as f64 * size / f.upem as f64)
    })();
    measured.unwrap_or(size * 0.55)
}

/// The matches of `q` (folded) in one run, whose last glyph advances `last` px.
fn matches_in(r: &crate::Run, last: f64, q: &[char], out: &mut Vec<[f32; 4]>, max: usize) {
    let chars: Vec<char> = r.text.chars().collect();
    // Folding can change the length (ß, İ): only fold when it keeps it,
    // so indexes still name the run's characters.
    let folded: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    if folded.len() < q.len() {
        return;
    }
    let n = chars.len();
    let size = r.size as f64 / PX;
    // Where each character starts: the glyph's offset when there is one
    // glyph per character, else in proportion along the run.
    let width = r.glyphs.last().map_or(0.0, |g| g.1 as f64 / PX) + last;
    let at = |i: usize| -> f64 {
        if r.glyphs.len() == n {
            if i < n { r.glyphs[i].1 as f64 / PX } else { width }
        } else {
            width * i as f64 / n.max(1) as f64
        }
    };
    let mut i = 0;
    while i + q.len() <= n {
        if folded[i..i + q.len()] == *q {
            let x0 = r.x as f64 / PX + at(i);
            let x1 = r.x as f64 / PX + at(i + q.len());
            let top = r.y as f64 / PX - size * 0.85;
            out.push([x0 as f32, top as f32, (x1 - x0).max(1.0) as f32, (size * 1.1) as f32]);
            if out.len() >= max {
                return;
            }
            i += q.len();
        } else {
            i += 1;
        }
    }
}

/// The top of the element whose `id` is `id` in `html` laid out at `env`,
/// in CSS px from the document's top (an in-page link's target,
/// navigator-ui.md §5.7), and the document's height. `None` when no
/// laid-out element has that id.
pub fn element_top(html: &str, fonts: &FontSet, env: &Env, subs: &Subresources, id: &str) -> Option<(f32, f32)> {
    // Mark the element for the geometry pass. The normalizer writes
    // attributes double-quoted and escaped, so an id that needs escaping
    // is not looked for (the parent only names ids the page has).
    if id.is_empty() || id.contains(['"', '&', '<', '>']) {
        return None;
    }
    let needle = format!(" id=\"{id}\"");
    let at = html.find(&needle)? + needle.len();
    let marked = format!("{} atrium-seek=\"1\"{}", &html[..at], &html[at..]);
    let g = crate::html::layout_geometry(&marked, fonts, env, subs, "atrium-seek");
    g.elements.get(&1).map(|e| ((e.y as f64 / PX) as f32, (g.height as f64 / PX) as f32))
}
