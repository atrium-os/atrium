//! ★ THE DISPLAY PATH IN THE WORKER (backend §4.8, bands step 1): from the
//! session's document to the scene of its first screen.
//!
//! Normalizing is about half of a big document's worker peak and seconds of
//! its time (§4.8, "where a big document's worker memory is"), and it is a
//! pure function of the recording. So it is done once per recording: the
//! first worker to show a recording's untouched initial state normalizes it
//! and hands the normalized document back with the scene; the broker stores
//! it by content address and gives it to every later worker showing that
//! same state, which then only lays out.
//!
//! A navigated document is a changed DOM, not the recording's: it is
//! normalized again from its current serialization, with the recording's
//! inputs (stylesheets, image sizes, sprites), and never cached.
//!
//! What a worker answers is the first screen only — `render_html_first` down
//! to the limit, then exactly the nodes that paint in `[0, limit)` — so an
//! answer is bounded by a screen, not by the document.

use navigator_render::{bands, fontset::FontSet, html};
use navigator_style::cascade::Env;

thread_local! {
    /// The pinned font set, loaded once per worker (loading canonicalizes
    /// every embedded face).
    static FONTS: std::cell::OnceCell<Result<FontSet, String>> = const { std::cell::OnceCell::new() };
}

fn with_fonts<T>(f: impl FnOnce(&FontSet) -> T) -> Result<T, String> {
    FONTS.with(|c| match c.get_or_init(|| FontSet::load().map_err(|e| format!("font set: {e}"))) {
        Ok(fonts) => Ok(f(fonts)),
        Err(e) => Err(e.clone()),
    })
}

/// What a worker answers for a scene request.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneAnswer {
    /// The first screen as NSG text.
    pub nsg: String,
    /// The normalized document, when this worker produced it for the
    /// recording's untouched initial state (for the broker to store).
    pub normalized: Option<Vec<u8>>,
}

/// The largest first-screen limit a request may ask for, px.
pub const MAX_LIMIT_PX: u32 = 8192;

/// The first screen of a document, down to `limit_px`.
///
/// - `recording`: the session's recording bytes (its normalizer inputs);
/// - `current`: the document as it is now, serialized — `None` when it is
///   the recording's untouched initial state;
/// - `cached`: a normalized document the broker holds for that initial
///   state (ignored for a navigated document).
pub fn first_screen(recording: &[u8], current: Option<&str>, cached: Option<&[u8]>, limit_px: u32) -> Result<SceneAnswer, String> {
    if limit_px == 0 || limit_px > MAX_LIMIT_PX { return Err(format!("limit {limit_px} px is outside 1..={MAX_LIMIT_PX}")) }
    let json = std::str::from_utf8(recording).map_err(|_| "the recording is not UTF-8")?;
    let r = navigator_normalize::recording::parse(json)?;
    let env = Env::default();
    let limit = limit_px as i64 * 64;
    with_fonts(|fonts| {
        let (normalized, produced) = match (current, cached) {
            (None, Some(c)) => (String::from_utf8(c.to_vec()).map_err(|_| "the cached normalized document is not UTF-8")?, false),
            (None, None) => (navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, &env).0, true),
            (Some(doc), _) => (navigator_normalize::normalize_and_measure(doc, &r.inputs, fonts, &env).0, false),
        };
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let (out, _) = html::render_html_first(&normalized, fonts, &env, &subs, limit);
        // Exactly what paints in the first screen, whatever the layout laid
        // out (it lays a page out whole when the first screen is unsafe).
        let idx = bands::index(&out.scene, limit);
        let mut first = bands::scene_of(&out.scene, &idx.nodes_in(0, limit));
        first.height = first.height.min(limit);
        let nsg = navigator_render::nsg::write(&first, fonts);
        Ok(SceneAnswer { nsg, normalized: produced.then(|| normalized.into_bytes()) })
    })?
}
