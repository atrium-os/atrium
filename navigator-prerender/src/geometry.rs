//! ★★ LAYOUT GEOMETRY FOR THE STATE RECORDER (legacy-web §5.4.5.6e).
//!
//! A page's scripts ask where things are: `getBoundingClientRect`,
//! `offsetWidth`, an `IntersectionObserver`'s first record. The converter
//! used to answer every one of those with the same fixed box (1280 × 600 at
//! the origin), which made a scroll-spy mark the LAST header and an
//! equaliser write `min-height: 600px` onto headlines. Here they are
//! answered from a real layout instead — and from the SAME layout the
//! document is later drawn with: the live DOM is serialized, normalized and
//! laid out by navigator-normalize and navigator-render, the lane's own
//! stages, not a second layout engine that would disagree with them.
//!
//! How the answers find their elements: every element is serialized with
//! `data-atrium-geo="<its handle>"` (`Dom::serialize_marked`); the
//! normalizer keeps attributes, and the renderer reports each laid-out box
//! by that attribute's value. Node POSITIONS do not survive normalization
//! (row-reverse reorders children, whitespace is stripped); a marker does.
//!
//! ★ Deterministic: the normalizer and renderer are pure functions of the
//! document, the pinned fonts and the viewport, and every budget in them
//! counts work, not time (threat model §2.6). When a layout happens is a
//! function of the script's reads and mutations, which are deterministic
//! too. The number of layouts per conversion is bounded (`MAX_LAYOUTS`),
//! by count, never by time.

use crate::fetch::Fetcher;
use crate::subresource::collect_for_layout;
use navigator_dom::Dom;
use navigator_render::fontset::FontSet;
use navigator_render::html::{layout_geometry, Geometry};
use navigator_style::cascade::Env;

/// The marker attribute. Nothing in a page's CSS names it.
pub const MARK: &str = "data-atrium-geo";

/// ★ The conversion viewport: what the converter already tells scripts
/// (`innerWidth` × `innerHeight`, and the nominal metrics it used to
/// answer with). A geometry answer at another width would contradict the
/// page's own view of its window.
pub const VIEWPORT: (f64, f64) = (1280.0, 600.0);

/// Layouts per conversion, at most. The first answers load-time reads; a
/// read after a mutation lays out again while this lasts, and after it the
/// last layout answers (`stale` in the report). Measured on both corpora
/// before it was chosen (legacy-web §5.4.5.6e, as built).
pub const MAX_LAYOUTS: u32 = 8;

thread_local! {
    /// The pinned font set, canonicalized once per process: it is compiled
    /// in, so this cannot fail on one machine and not another.
    static FONTS: std::cell::OnceCell<Result<FontSet, String>> = const { std::cell::OnceCell::new() };
}

/// The environment the layout runs in: the conversion viewport, light,
/// no reduced motion, 1 dppx — the converter's identity, not a reader's.
pub fn env() -> Env {
    Env { width_px: VIEWPORT.0, height_px: VIEWPORT.1, ..Env::default() }
}

/// Lay the current document out. An error if the font set cannot be
/// loaded (the caller then keeps the old nominal answers and counts it).
pub fn layout(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher) -> Result<Geometry, String> {
    let html = dom.serialize_marked(MARK);
    let subs = collect_for_layout(dom, base, fetcher);
    let mut inputs = navigator_normalize::Inputs::default();
    for s in &subs.sheets {
        inputs.stylesheets.insert(s.href.clone(), s.text.clone());
        if !s.media.is_empty() { inputs.stylesheet_media.insert(s.href.clone(), s.media.clone()); }
    }
    for i in &subs.images {
        inputs.images.insert(i.src.clone(), navigator_normalize::ImageSize {
            width: i.natural.width, height: i.natural.height, ratio: i.natural.ratio });
    }
    // ★ An image the converter could not size (no `src` yet — a lazy
    // placeholder — or one that failed) still has a BOX in a browser: its
    // CSS size, or its `width`/`height` attributes. The normalizer drops an
    // unsized image from the document, which is right for the artifact
    // (profile §3.13) and wrong for a layout that answers a script: Samsung's
    // loader asks `offsetWidth || getClientRects().length` before loading,
    // and an image with no box is never loaded. So for the LAYOUT only,
    // every unsized image is declared 0 × 0 with no ratio — what Chrome lays
    // a source-less image out as (measured: `width: 100%` in a 300 px column
    // gives 300 × 0, no CSS gives 0 × 0, and each has one client rect), not
    // the 300 × 150 default object size, which would push everything below
    // it 150 px down.
    for h in dom.by_tag_anywhere("img") {
        let src = dom.attr(h, "src").unwrap_or("").to_string();
        inputs.images.entry(src).or_insert(navigator_normalize::ImageSize { width: Some(0), height: Some(0), ratio: None });
    }
    FONTS.with(|f| {
        let fonts = f.get_or_init(FontSet::load).as_ref().map_err(|e| e.clone())?;
        let env = env();
        let (normalized, _) = navigator_normalize::normalize_and_measure(&html, &inputs, fonts, &env);
        Ok(layout_geometry(&normalized, fonts, &env, &navigator_render::Subresources::new(), MARK))
    })
}
