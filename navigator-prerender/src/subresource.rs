//! Subresources the NORMALIZER needs — collected by the converter, because
//! the normalizer has no capabilities (Profile v1 §6) and the renderer has
//! no measurement path (§3.13).
//!
//! Two kinds, and they are needed for different reasons:
//! - **stylesheets**, because a document's design is in them and a
//!   normalizer that is not given them emits a page with no styling at all;
//! - **image intrinsic sizes**, because layout must know a replaced box's
//!   size BEFORE it lays out (§1.2, the anti-CLS rule), and nothing later in
//!   the lane is allowed to measure.
//!
//! ★ The size is read from the image's own header, and the bytes are named
//! by their content address — so the renderer can paint from CAS without
//! anyone re-fetching, and a byte that changes changes the address.

use crate::fetch::Fetcher;
use navigator_dom::Dom;

#[derive(Debug, Clone, PartialEq)]
pub struct Sheet { pub href: String, pub media: String, pub text: String }

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub src: String,
    pub natural: Natural,
    pub address: String,
    /// ★ Set only in a first-display recording whose images have not been
    /// fetched yet (legacy-web §5.4.5.7, the image-free first display):
    /// what its box is sized from until the image arrives. `None` is an
    /// image that HAS arrived (or failed), and is how every other recording
    /// is written — the field is not emitted for it, so those recordings
    /// are byte for byte what they were.
    pub pending: Option<Pending>,
}

/// How a not-yet-fetched image's box is sized at first display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The document declares both `width` and `height`: the box is that
    /// size, as it will be once the image arrives (unless the page's CSS
    /// takes its height from the image's own ratio, and that ratio differs).
    Declared,
    /// It does not: no natural size is claimed (`null`, ratio `none`), so
    /// the box is whatever the page's CSS makes it, else the default object
    /// size (300 × 150, CSS Images 3 §5.3). Its arrival may move the layout,
    /// and that is measured, not assumed away.
    Placeholder,
    /// ★ It does not, and its natural size was read from the image's own
    /// leading bytes (cut A, `navigator_dom::natural::header_size`): exactly
    /// the size the whole image will be given, so its box is laid out as it
    /// will be and the image's arrival moves nothing.
    Header,
}

impl Pending {
    pub fn as_str(self) -> &'static str {
        match self { Pending::Declared => "declared", Pending::Placeholder => "placeholder", Pending::Header => "header" }
    }
}

/// Whether the first display would size `<img>` `h` with no fetch at all
/// (both `width` and `height` given): the rule `collect_first` applies, and
/// the one the converter's header hints follow.
/// A tracking pixel: an `<img>` that declares both dimensions at most 1 px,
/// or carries the `hidden` attribute itself (old.reddit's is
/// `<img src="https://id.rlcdn.com/…gif" hidden>`). Never fetched.
/// Subresources are collected from the document AFTER its scripts ran, so
/// an image a script revealed has lost its `hidden` by then. An image
/// hidden by CSS or inside a hidden container is still fetched: that is
/// how a carousel's later slides are hidden until a transition shows them.
pub fn invisible_img(dom: &Dom, h: navigator_dom::Handle) -> bool {
    let declared = |n: &str| dom.attr(h, n).and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok());
    dom.attr(h, "hidden").is_some()
        || matches!((declared("width"), declared("height")), (Some(w), Some(ht)) if w <= 1 && ht <= 1)
}

pub fn declares_size(dom: &Dom, h: navigator_dom::Handle) -> bool {
    let declared = |n: &str| dom.attr(h, n).and_then(|v| v.trim().parse::<u32>().ok());
    declared("width").is_some() && declared("height").is_some()
}

/// A header miss, for the report: `late` (not in hand when the header
/// window closed), `fetch` (the request failed), or what the parser said.
fn miss_kind(why: &str) -> String {
    if why.contains("not in time") { return "late".into() }
    if why.starts_with("fetch ") || why.starts_with("peek ") || why.contains("HTTP ") || why.contains("refused") || why.contains("offline") { return "fetch".into() }
    why.split_whitespace().take(4).collect::<Vec<_>>().join("_").replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "")
}

/// ★ Whether a header miss PROVES the image will fail when its body is
/// fetched — so the first display leaves it out, as the final recording
/// will, instead of giving it a box that will collapse:
/// - the parser saw enough to know `natural_size` will say nothing (no
///   raster magic, no `<svg` in the first 4 KiB: `header_size`'s "not an
///   image read here" is decided on exactly what `natural_size` reads);
/// - the server answered an error status, the address rule or the budget
///   refused it, or (the measurement instrument) it is not in the offline
///   cache: the body's request is the same request and is answered the same.
/// A late header, a timeout, or a connection that failed is not proof.
fn miss_proves_failure(why: &str) -> bool {
    let lower = why.to_ascii_lowercase();
    why == "not an image read here"
        || why.contains(": HTTP 4") || why.contains(": HTTP 5")
        || lower.contains("refused") || why.contains("offline, not cached")
}

/// ★ An image's natural size from its leading bytes, asked for as
/// `header_size` asks (at most `MAX_HEADER_BYTES`, at most four reads), or
/// `None` with the reason. The bytes are hostile and are read by the strict
/// parser only; they are never stored, hashed or kept.
pub fn size_from_header(url: &str, fetcher: &mut dyn Fetcher) -> Result<Natural, String> {
    use navigator_dom::natural::{header_size, next_header_read, Header};
    let mut have: Vec<u8> = Vec::new();
    let mut ended = false;
    let mut want = next_header_read(0, 0);
    loop {
        let ask = want - have.len();
        let got = fetcher.peek(url, have.len(), ask)?;
        if got.len() > ask { return Err("a peek answered more than was asked".into()) }
        ended = ended || got.len() < ask;
        have.extend_from_slice(&got);
        match header_size(&have, ended) {
            Header::Sized(n) => return Ok(n),
            Header::Unreadable(why) => return Err(why.to_string()),
            Header::More(n) => {
                if ended { return Err("the image ends inside its header".into()) }
                let next = next_header_read(have.len(), n);
                if next <= have.len() { return Err("the header runs past 64 KiB".into()) }
                want = next;
            }
        }
    }
}

/// An image's natural sizing; the reader lives in `navigator_dom::natural`
/// so the WPT oracle can use it without depending on the converter.
pub use navigator_dom::natural::{natural_size, Natural};

/// Where fetched bytes are kept, named by their content address. A
/// stand-in for Tessera's CAS with the same two properties that matter: the
/// name IS the hash, so a byte that changes changes the name, and a second
/// document referencing the same image costs nothing.
pub fn blob_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PRERENDER_BLOB_DIR").map(std::path::PathBuf::from)
}

/// ★ The jailed converter has no filesystem, so its blobs go to its parent
/// over stdout instead (src/serve.rs). When a sink is installed it takes
/// EVERY blob and the environment is not consulted at all, so an unconfined
/// serve-mode child behaves exactly like a jailed one.
static SINK: std::sync::Mutex<Option<Vec<(String, Vec<u8>)>>> = std::sync::Mutex::new(None);

pub fn install_blob_sink() { *SINK.lock().unwrap() = Some(vec![]) }

/// Everything the sink collected, in first-seen order, each address once.
pub fn take_blobs() -> Vec<(String, Vec<u8>)> {
    SINK.lock().unwrap().as_mut().map(std::mem::take).unwrap_or_default()
}

fn store(address: &str, bytes: &[u8]) {
    if let Some(sink) = SINK.lock().unwrap().as_mut() {
        if address.starts_with("blake3:") && !sink.iter().any(|(a, _)| a == address) {
            sink.push((address.to_string(), bytes.to_vec()));
        }
        return;
    }
    let Some(dir) = blob_dir() else { return };
    let Some(hex) = address.strip_prefix("blake3:") else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(hex);
    // Content-addressed: if it is there, it is already the right bytes.
    if path.exists() { return }
    let _ = std::fs::write(path, bytes);
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Subresources {
    pub sheets: Vec<Sheet>,
    pub images: Vec<Image>,
    /// ★ External SVG sprite files: the file part of an inline
    /// `<use href="file.svg#icon">` as the document writes it, and the file's
    /// text (`navigator_normalize::Inputs::sprites`). Carried so the
    /// normalizer can copy the symbol in and the icon stays self-contained.
    pub sprites: Vec<(String, String)>,
    /// What could not be fetched or measured, with the reason.
    pub failed: Vec<(String, String)>,
}

/// The profile bounds `@import` depth (§3.11); so does this.
const MAX_IMPORT_DEPTH: usize = 2;

pub fn collect(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher) -> Subresources {
    collect_inner(dom, base, fetcher, Mode::Final, None)
}

/// ★ THE IMAGE-FREE FIRST DISPLAY (legacy-web §5.4.5.7): the stylesheets,
/// fetched as `collect` fetches them, and the images NOT fetched — each
/// entered as `Pending`, sized from the document's own `width`/`height`
/// where it declares both, otherwise claiming no natural size at all. A
/// first display then waits for the page's design and not for its
/// pictures, and every image keeps a box (the normalizer drops an `<img>`
/// it has no entry for, which would move the layout when it arrived).
pub fn collect_first(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher) -> Subresources {
    collect_inner(dom, base, fetcher, Mode::First, None)
}

/// `collect`, offering `progress` after each image what the recording's
/// images would be if the collection stopped there: every image handled so
/// far as `collect` writes it, and every one not yet handled as `first`
/// has it (`collect_first`). The list is built only if `progress` asks for
/// it (calls the function it is handed), since most offers are declined.
/// The returned value, not the last offer, is the final list: an image that
/// failed once and was found again later in the document lands in its
/// final place only there.
pub fn collect_streaming(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher, first: &[Image],
                         progress: &mut dyn FnMut(&dyn Fn() -> Vec<Image>)) -> Subresources {
    let mut p = |done: &Subresources, handled: &std::collections::HashSet<String>| {
        let now = || -> Vec<Image> {
            first.iter().filter_map(|f| {
                if !handled.contains(&f.src) { return Some(f.clone()) }
                done.images.iter().find(|i| i.src == f.src).cloned()
            }).collect()
        };
        progress(&now);
    };
    collect_inner(dom, base, fetcher, Mode::Final, Some(&mut p))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The images a recording names: fetched, measured, hashed and stored.
    Final,
    /// A layout's: measured only (`collect_for_layout`).
    Layout,
    /// First display's: not fetched (`collect_first`).
    First,
}

/// ★ Every subresource fetched during ONE conversion, by absolute URL —
/// layouts (legacy-web §5.4.5.6e) and the final collection share it, so a
/// sheet or an image crosses the fetcher (in a jail: the relay, whose bytes
/// are budgeted per conversion) once. Fetching each again for the final
/// collection pushed The Verge's and The Hindu's images past the relay's
/// budget, and the recording lost them. Cleared at the start and end of a
/// conversion (`forget_fetched`). It holds no more than the blob sink
/// already does: every image a recording names.
type Fetched = (std::collections::HashMap<String, Result<String, String>>,
                std::collections::HashMap<String, Result<std::rc::Rc<Vec<u8>>, String>>);
thread_local! {
    static FETCHED: std::cell::RefCell<Fetched> = std::cell::RefCell::new(Default::default());
}

pub fn forget_fetched() { FETCHED.with(|f| *f.borrow_mut() = Default::default()) }

/// ★ The inputs a layout of the CURRENT document needs, by the same walk as
/// `collect` — the sheets and image sizes the recording would carry if the
/// document stopped here — but with no side effects on the artifact: no
/// blob is stored and nothing is hashed, so laying a page out mid-script
/// cannot add a blob the final document does not name.
pub fn collect_for_layout(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher) -> Subresources {
    collect_inner(dom, base, fetcher, Mode::Layout, None)
}

/// A fetcher that answers from `FETCHED` first.
struct Remembering<'a> { inner: &'a mut dyn Fetcher }
impl Fetcher for Remembering<'_> {
    fn get(&mut self, url: &str) -> Result<String, String> {
        if let Some(r) = FETCHED.with(|f| f.borrow().0.get(url).cloned()) { return r }
        let r = self.inner.get(url);
        FETCHED.with(|f| f.borrow_mut().0.insert(url.to_string(), r.clone()));
        r
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if let Some(r) = FETCHED.with(|f| f.borrow().1.get(url).cloned()) { return r.map(|b| (*b).clone()) }
        let r = self.inner.get_bytes(url).map(std::rc::Rc::new);
        FETCHED.with(|f| f.borrow_mut().1.insert(url.to_string(), r.clone()));
        r.map(|b| (*b).clone())
    }
    fn hint(&mut self, url: &str) {
        if FETCHED.with(|f| f.borrow().0.contains_key(url)) { return }
        self.inner.hint(url)
    }
    /// From the whole body when this conversion has it; otherwise asked of
    /// the fetcher, and NOT remembered: a header is not the body, and the
    /// request for the body, when it comes, is the same request as before.
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> {
        if let Some(Ok(b)) = FETCHED.with(|f| f.borrow().1.get(url).cloned()) {
            let a = start.min(b.len());
            return Ok(b[a..(a + len).min(b.len())].to_vec());
        }
        self.inner.peek(url, start, len)
    }
}

type Progress<'a> = &'a mut dyn FnMut(&Subresources, &std::collections::HashSet<String>);

fn collect_inner(dom: &Dom, base: Option<&str>, fetcher: &mut dyn Fetcher, mode: Mode,
                 mut progress: Option<Progress<'_>>) -> Subresources {
    let layout = mode == Mode::Layout;
    let mut remembering = Remembering { inner: fetcher };
    let fetcher: &mut dyn Fetcher = &mut remembering;
    let mut out = Subresources::default();
    let resolve = |href: &str| -> Option<String> {
        match base.and_then(|b| url::Url::parse(b).ok()) {
            Some(b) => b.join(href).ok().map(|u| u.to_string()),
            None => href.starts_with("http").then(|| href.to_string()),
        }
    };

    // ★ IMPORTS EARLY (legacy-web §5.4.5.7, cut B). The sheets were hinted
    // together, so they arrive in parallel; an `@import` was found only
    // when the walk below reached its sheet, and fetched then, alone. So
    // first take every sheet as it arrives, hinting its imports at once,
    // then theirs, to the profile's depth; the walk below then finds them
    // all fetched or in flight. It is the same walk over the same bytes
    // (the conversion's memo), so what it collects is unchanged.
    let mut level: Vec<String> = vec![];
    for h in dom.by_tag_anywhere("link") {
        let is_sheet = dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")));
        if let (true, Some(abs)) = (is_sheet, dom.attr(h, "href").and_then(|href| resolve(href))) {
            if !level.contains(&abs) { level.push(abs) }
        }
    }
    for _ in 0..MAX_IMPORT_DEPTH {
        let mut next = vec![];
        for abs in &level {
            let Ok(text) = fetcher.get(abs) else { continue };
            for t in import_targets(&text) {
                let Ok(u) = url::Url::parse(abs).and_then(|b| b.join(&t)) else { continue };
                let u = u.to_string();
                if !next.contains(&u) { fetcher.hint(&u); next.push(u) }
            }
        }
        level = next;
    }

    for h in dom.by_tag_anywhere("link") {
        let is_sheet = dom.attr(h, "rel").is_some_and(|r| r.split_ascii_whitespace().any(|x| x.eq_ignore_ascii_case("stylesheet")));
        if !is_sheet { continue }
        let Some(href) = dom.attr(h, "href").map(str::to_string) else { continue };
        if out.sheets.iter().any(|s| s.href == href) { continue }
        let Some(abs) = resolve(&href) else { out.failed.push((href, "cannot resolve".into())); continue };
        match fetcher.get(&abs) {
            Ok(text) => {
                // ★ Follow `@import`: the W3C's stylesheet for a spec is 123
                // bytes and one import. A converter that stops at the link
                // hands the normalizer an empty design.
                let media = dom.attr(h, "media").unwrap_or("").to_string();
                imports(&text, &abs, &href, fetcher, 0, &mut out);
                out.sheets.push(Sheet { href, media, text });
            }
            Err(e) => out.failed.push((href, e)),
        }
    }

    // Every `src` met, whatever became of it: what `collect_streaming`
    // reports progress against. (`out.images` alone cannot say, because an
    // image that failed is not in it.)
    let mut handled: std::collections::HashSet<String> = Default::default();
    for h in dom.by_tag_anywhere("img") {
        let Some(src) = dom.attr(h, "src").map(str::to_string) else { continue };
        // ★ Deduplicated by what is IN the list, as it always was: a later
        // `<img>` with the same `src` is tried again when the first one
        // failed (it may declare a size the first did not).
        if out.images.iter().any(|i| i.src == src) { continue }
        handled.insert(src.clone());
        // An image whose size the DOCUMENT already declares needs no fetch:
        // it has already arrived measured.
        let declared = |n: &str| dom.attr(h, n).and_then(|v| v.trim().parse::<u32>().ok());
        let Some(abs) = resolve(&src) else { out.failed.push((src, "cannot resolve".into())); continue };
        // ★ A PIXEL IS NEVER FETCHED (navigator-ui §6.6): an image that
        // declares itself at most 1 × 1, or is `hidden` itself, shows nothing a reader could see, so
        // its only effect would be telling its host a conversion happened.
        // It keeps its declared size, with no bytes. Structural, like
        // §5.4.1f's script rule: no host list.
        if invisible_img(dom, h) {
            let natural = Natural::sized(declared("width").unwrap_or(0), declared("height").unwrap_or(0));
            out.images.push(Image { src, natural, address: String::new(), pending: None });
            crate::timing::count("img.pixel_not_fetched");
            continue;
        }
        if mode == Mode::First {
            // ★ Cut A: an image the page does not size is sized from its
            // own header, when its leading bytes are in hand in time (the
            // fetcher hinted them with the stylesheets). Otherwise it stays
            // a placeholder, and the reason is reported.
            let (natural, pending) = match declared("width").zip(declared("height")) {
                Some((w, ht)) => (Natural::sized(w, ht), Pending::Declared),
                None => {
                    let _s = crate::timing::span("header");
                    match size_from_header(&abs, fetcher) {
                        Ok(n) => { crate::timing::count("header.sized"); (n, Pending::Header) }
                        Err(why) if miss_proves_failure(&why) => {
                            // Left out, as the final recording will leave it.
                            crate::timing::count(&format!("header.fails.{}", miss_kind(&why)));
                            out.failed.push((src, format!("header: {why}")));
                            continue;
                        }
                        Err(why) => {
                            crate::timing::count(&format!("header.{}", miss_kind(&why)));
                            out.failed.push((src.clone(), format!("header: {why}")));
                            (Natural { width: None, height: None, ratio: None }, Pending::Placeholder)
                        }
                    }
                }
            };
            out.images.push(Image { src, natural, address: String::new(), pending: Some(pending) });
            continue;
        }
        // ★ For a layout, the SIZE is all that is needed: never hashed,
        // never stored.
        if layout {
            let n = fetcher.get_bytes(&abs).ok().and_then(|b| natural_size(&b));
            match n.or_else(|| declared("width").zip(declared("height")).map(|(w, h)| Natural::sized(w, h))) {
                Some(natural) => out.images.push(Image { src, natural, address: String::new(), pending: None }),
                None => out.failed.push((src, "no intrinsic size".into())),
            }
            continue;
        }
        match fetcher.get_bytes(&abs) {
            Ok(bytes) => match natural_size(&bytes).or_else(|| declared("width").zip(declared("height")).map(|(w, h)| Natural::sized(w, h))) {
                Some(natural) => {
                    let address = format!("blake3:{}", blake3::hash(&bytes).to_hex());
                    store(&address, &bytes);
                    out.images.push(Image { src, natural, address, pending: None });
                }
                None => out.failed.push((src, "no intrinsic size in the header".into())),
            },
            Err(e) => match declared("width").zip(declared("height")) {
                // The document declared it, so layout is safe even though the
                // bytes are not in hand: the renderer reserves the space.
                Some((w, ht)) => out.images.push(Image { src, natural: Natural::sized(w, ht), address: String::new(), pending: None }),
                None => out.failed.push((src, e)),
            },
        }
        if let Some(p) = progress.as_mut() { p(&out, &handled) }
    }
    if !layout {
        // ★ Not for the FIRST display (legacy-web §5.4.5.7): a sprite file
        // is a whole round trip that only draws icons, and the first display
        // does not wait for its images either. Its icons fill in with the
        // readable recording.
        if mode != Mode::First { sprites(dom, &resolve, fetcher, &mut out) }
        inline_svgs(dom, &mut out);
    }
    out
}

/// ★ EXTERNAL SVG SPRITE FILES (2026-11-06; approved by the user). Every
/// file an inline `<use href="file.svg#id">` names, fetched once — through
/// the same fetcher as the stylesheets, so in the jailed lane through fetchd
/// with its origin and size rules — at most `MAX_SPRITE_FILES` of them and
/// `MAX_SPRITE_BYTES` each. Hinted together first, so they arrive in
/// parallel. Keyed by the href as the document writes it, as stylesheets are.
fn sprites(dom: &Dom, resolve: &dyn Fn(&str) -> Option<String>, fetcher: &mut dyn Fetcher, out: &mut Subresources) {
    use navigator_normalize::{MAX_SPRITE_BYTES, MAX_SPRITE_FILES};
    let mut files: Vec<(String, String)> = vec![];
    for h in dom.by_tag_anywhere("use") {
        let Some(v) = dom.attr(h, "href").or_else(|| dom.attr(h, "xlink:href")) else { continue };
        let Some((file, id)) = v.split_once('#') else { continue };
        if file.is_empty() || id.is_empty() || files.iter().any(|(f, _)| f == file) { continue }
        if files.len() >= MAX_SPRITE_FILES { out.failed.push((file.to_string(), "sprite: over the per-document file count".into())); continue }
        match resolve(file) {
            Some(abs) => files.push((file.to_string(), abs)),
            None => out.failed.push((file.to_string(), "sprite: cannot resolve".into())),
        }
    }
    for (_, abs) in &files { fetcher.hint(abs) }
    for (file, abs) in files {
        match fetcher.get(&abs) {
            Ok(text) if text.len() > MAX_SPRITE_BYTES => out.failed.push((file, format!("sprite: {} bytes, over {MAX_SPRITE_BYTES}", text.len()))),
            Ok(text) => out.sprites.push((file, text)),
            Err(e) => out.failed.push((file, format!("sprite: {e}"))),
        }
    }
}

/// ★ Inline `<svg>`s, made standalone (threat model §2.4; the function is
/// `navigator_normalize::svg_inline::standalone`, which the normalizer runs
/// again on the recording and must agree with byte for byte). Each becomes a
/// blob named by its content address and an image entry under that address,
/// sized from its own header; the normalizer then turns the `<svg>` box into
/// a replaced element painting it. Nothing is fetched, so nothing is
/// pending. The inputs are built exactly as `recording::parse` builds them
/// from the sheets this recording carries, and the document is the
/// serialization the recording carries.
fn inline_svgs(dom: &Dom, out: &mut Subresources) {
    if dom.by_tag_anywhere("svg").is_empty() { return }
    let mut inputs = navigator_normalize::Inputs::default();
    for sh in &out.sheets {
        inputs.stylesheets.insert(sh.href.clone(), sh.text.clone());
        if !sh.media.is_empty() { inputs.stylesheet_media.insert(sh.href.clone(), sh.media.clone()); }
    }
    for (file, text) in &out.sprites { inputs.sprites.insert(file.clone(), text.clone()); }
    for text in navigator_normalize::svg_inline::standalone(&dom.serialize(), &inputs) {
        let bytes = text.into_bytes();
        let address = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        if out.images.iter().any(|i| i.src == address) { continue }
        let Some(natural) = natural_size(&bytes) else { continue };
        store(&address, &bytes);
        out.images.push(Image { src: address.clone(), natural, address, pending: None });
    }
}

/// `abs` is where the sheet was fetched from; `key` is how the DOCUMENT
/// names it. ★ The two differ, and the import must be keyed the way the
/// normalizer will resolve it — relative to the importing sheet's own key —
/// or the lookup misses and the imported CSS is silently absent.
/// The targets of a sheet's `@import`s, in order, as `imports` reads them.
fn import_targets(css: &str) -> Vec<String> {
    if !css.contains("@import") { return vec![] }
    css.split("@import").skip(1).filter_map(|stmt| {
        // ★ At most 256 bytes when there is no `;` — cut on a character
        // boundary: a byte index inside a multi-byte character panicked.
        let cut = stmt.find(';').unwrap_or_else(|| (0..=stmt.len().min(256)).rev().find(|&i| stmt.is_char_boundary(i)).unwrap_or(0));
        let head = &stmt[..cut];
        let target = head.split_once('"').map(|(_, r)| r.split('"').next().unwrap_or("").to_string())
            .or_else(|| head.split_once("url(").map(|(_, r)| r.split(')').next().unwrap_or("").trim_matches(['"', '\'']).to_string()))
            .unwrap_or_default();
        (!target.is_empty()).then_some(target)
    }).collect()
}

fn imports(css: &str, abs: &str, key: &str, fetcher: &mut dyn Fetcher, depth: usize, out: &mut Subresources) {
    if depth >= MAX_IMPORT_DEPTH { return }
    for target in import_targets(css) {
        let Ok(next_abs) = url::Url::parse(abs).and_then(|b| b.join(&target)) else { continue };
        let next_abs = next_abs.to_string();
        let next_key = match (target.starts_with("http"), key.rfind('/')) {
            (true, _) => target.clone(),
            (false, Some(i)) => format!("{}{}", &key[..i + 1], target),
            (false, None) => target.clone(),
        };
        if out.sheets.iter().any(|s| s.href == next_key) { continue }
        match fetcher.get(&next_abs) {
            Ok(text) => {
                imports(&text, &next_abs, &next_key, fetcher, depth + 1, out);
                out.sheets.push(Sheet { href: next_key, media: String::new(), text });
            }
            Err(e) => out.failed.push((next_key, e)),
        }
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;

    #[test]
    fn import_targets_read_both_forms_and_a_hostile_tail_does_not_panic() {
        assert_eq!(import_targets("@import \"a.css\"; @import url(b.css) screen; p{}"), vec!["a.css", "b.css"]);
        // No `;`, and byte 256 falls inside a multi-byte character.
        let hostile = format!("@import {}é{}", "x".repeat(248), "é".repeat(200));
        let _ = import_targets(&hostile);
        assert!(import_targets("p { color: red }").is_empty());
    }

    /// Sheets that arrive as asked; every request and hint logged in order.
    struct Log(Vec<String>);
    impl Fetcher for Log {
        fn get(&mut self, url: &str) -> Result<String, String> {
            self.0.push(format!("get {url}"));
            Ok(if url.ends_with("/a.css") { "@import \"imp.css\"; a{}".into() } else if url.ends_with("/imp.css") { "@import \"deep.css\";".into() } else { "b{}".into() })
        }
        fn hint(&mut self, url: &str) { self.0.push(format!("hint {url}")) }
    }

    #[test]
    fn imports_are_hinted_as_their_sheet_arrives_and_the_collection_is_unchanged() {
        forget_fetched();
        let dom = navigator_dom::parse(r#"<link rel=stylesheet href="/a.css"><link rel=stylesheet href="/b.css">"#);
        let mut f = Log(vec![]);
        let subs = collect_first(&dom, Some("https://s.example/p"), &mut f);
        forget_fetched();
        let at = |s: &str| f.0.iter().position(|l| l == s).unwrap_or_else(|| panic!("{s} not in {:?}", f.0));
        // The import is hinted before the second sheet is asked for, and
        // the nested one before the import is (the walk then asks for it).
        assert!(at("hint https://s.example/imp.css") < at("get https://s.example/b.css"), "{:?}", f.0);
        assert!(at("hint https://s.example/deep.css") < at("get https://s.example/deep.css"), "{:?}", f.0);
        // Each fetched once, and collected as the walk always collected them.
        assert_eq!(f.0.iter().filter(|l| l.starts_with("get ")).count(), 4, "{:?}", f.0);
        let hrefs: Vec<&str> = subs.sheets.iter().map(|s| s.href.as_str()).collect();
        assert_eq!(hrefs, vec!["/deep.css", "/imp.css", "/a.css", "/b.css"]);
    }
}

/// Intrinsic size from an image's own header. Formats the corpus actually
/// contains; anything else is reported rather than guessed at.
/// Both natural dimensions, when the image has both — the raster case.
pub fn intrinsic_size(b: &[u8]) -> Option<(u32, u32)> {
    let n = natural_size(b)?;
    n.width.zip(n.height)
}

