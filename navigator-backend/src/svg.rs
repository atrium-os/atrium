//! SVG as an image: a strict profile, then a rasteriser, in the worker
//! (threat model §2.4, the SVG rows).
//!
//! ★★ SVG IS A DOCUMENT FORMAT, NOT A PICTURE FORMAT. A browser's SVG can
//! run script, load other files, embed HTML (`foreignObject`), animate, and
//! expand a few hundred bytes into billions of nodes through `<use>`. None of
//! that is allowed here. The bytes are parsed once, by roxmltree, and the
//! tree is checked against the profile below; only a tree that passes is
//! handed (the same tree, not the bytes again) to usvg, and rendered by
//! resvg/tiny-skia into exactly the painted size.
//!
//! The profile, each limit set from the corpora's 279 distinct SVGs
//! (2026-11-06: bytes p50 2.4 kB, max 438 kB; elements p50 7, max 288; depth
//! max 9; path segments max 15,000; no script, foreignObject, filter or
//! external reference in any of them):
//! - at most `MAX_SVG_BYTES`, UTF-8, the root element `svg`;
//! - a bare DOCTYPE is dropped; an internal DTD subset or an entity is
//!   refused (roxmltree is run with DTDs off, so entity expansion cannot
//!   happen at all);
//! - SVG-namespace elements only from `ALLOWED`; `script`, `foreignObject`,
//!   `image`, text, filters, animation, `marker` and anything unlisted are
//!   refused BY NAME (a document drawn with a part silently missing is worse
//!   than one not drawn: v1 has no text, so an SVG with text is refused, not
//!   drawn without its words). Elements in other namespaces (editor
//!   metadata) are ignored by usvg and only counted;
//! - every reference is local: an `href` must be `#id`, every `url(…)` must
//!   be `url(#id)`, a `<style>` may not `@import` or `@font-face`, and no
//!   attribute may be an event handler;
//! - bounded: `MAX_ELEMENTS`, `MAX_DEPTH`, `MAX_PATH_SEGMENTS`,
//!   `MAX_STOPS`, the node count after `<use>` expansion
//!   (`MAX_EXPANDED_NODES`, computed with memoisation, a cycle refused), the
//!   nesting of layers that allocate (`MAX_LAYER_DEPTH`), and the work
//!   (expanded nodes × output pixels, `MAX_WORK`);
//! - pixels: the output is the painted size (vectors are drawn sharp at any
//!   size, so not reduced from a natural frame), checked against the axis
//!   cap and the document's decode budget BEFORE the pixmap is allocated.
//!   resvg's group layers allocate a pixmap each, so the transient frame is
//!   charged as output × (1 + layer depth).

use crate::image::{Ceilings, DecodeError, Format, Pixels, BYTES_PER_PIXEL};

pub use navigator_svg::profile::{MAX_SVG_BYTES, MAX_ELEMENTS, MAX_DEPTH, MAX_PATH_SEGMENTS, MAX_STOPS,
    MAX_EXPANDED_NODES, MAX_LAYER_DEPTH, MAX_WORK, Checked};

fn refuse(why: &'static str) -> DecodeError { DecodeError::Refused { format: Format::Svg, why } }

/// Rasterise an SVG at the painted size (`c.target`; else its own size;
/// else 300 × 150), under the profile and the ceilings.
pub fn rasterize(bytes: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    // The shared profile (navigator-svg): nothing outside the document can be
    // reached, no files, no data URLs, no fonts.
    let (tree, checked) = navigator_svg::load(bytes).map_err(|e| match e {
        navigator_svg::LoadError::Refused(why) => refuse(why),
        navigator_svg::LoadError::Corrupt(why) => DecodeError::Corrupt { format: Format::Svg, why },
    })?;
    let size = tree.size();
    let natural = (size.width().ceil().max(1.0) as u32, size.height().ceil().max(1.0) as u32);
    let (w, h) = c.target.unwrap_or(natural);
    if w == 0 || h == 0 { return Err(DecodeError::Empty { format: Format::Svg }) }
    if w > c.max_dimension || h > c.max_dimension { return Err(DecodeError::TooLarge { format: Format::Svg, width: w, height: h }) }
    let out = w as usize * h as usize * BYTES_PER_PIXEL;
    if out > c.budget { return Err(DecodeError::OverBudget { format: Format::Svg, needed: out, left: c.budget }) }
    let transient = out.saturating_mul(1 + checked.layer_depth);
    if transient > c.frame { return Err(DecodeError::FrameTooLarge { format: Format::Svg, needed: transient, limit: c.frame }) }
    if checked.expanded.saturating_mul(w as u64 * h as u64) > MAX_WORK { return Err(refuse("too much drawing work for its size")) }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or(DecodeError::Empty { format: Format::Svg })?;
    let t = resvg::tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height());
    resvg::render(&tree, t, &mut pixmap.as_mut());
    // tiny-skia is premultiplied; the worker's output is straight alpha.
    let mut rgba = Vec::with_capacity(out);
    for p in pixmap.pixels() {
        let d = p.demultiply();
        rgba.extend_from_slice(&[d.red(), d.green(), d.blue(), d.alpha()]);
    }
    Ok(Pixels { width: w, height: h, rgba })
}
