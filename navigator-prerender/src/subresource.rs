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
}

impl Pending {
    pub fn as_str(self) -> &'static str {
        match self { Pending::Declared => "declared", Pending::Placeholder => "placeholder" }
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
        if mode == Mode::First {
            let (natural, pending) = match declared("width").zip(declared("height")) {
                Some((w, ht)) => (Natural::sized(w, ht), Pending::Declared),
                None => (Natural { width: None, height: None, ratio: None }, Pending::Placeholder),
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
    out
}

/// `abs` is where the sheet was fetched from; `key` is how the DOCUMENT
/// names it. ★ The two differ, and the import must be keyed the way the
/// normalizer will resolve it — relative to the importing sheet's own key —
/// or the lookup misses and the imported CSS is silently absent.
fn imports(css: &str, abs: &str, key: &str, fetcher: &mut dyn Fetcher, depth: usize, out: &mut Subresources) {
    if depth >= MAX_IMPORT_DEPTH || !css.contains("@import") { return }
    for stmt in css.split("@import").skip(1) {
        let head = &stmt[..stmt.find(';').unwrap_or(stmt.len().min(256))];
        let target = head.split_once('"').map(|(_, r)| r.split('"').next().unwrap_or("").to_string())
            .or_else(|| head.split_once("url(").map(|(_, r)| r.split(')').next().unwrap_or("").trim_matches(['"', '\'']).to_string()))
            .unwrap_or_default();
        if target.is_empty() { continue }
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

/// Intrinsic size from an image's own header. Formats the corpus actually
/// contains; anything else is reported rather than guessed at.
/// Both natural dimensions, when the image has both — the raster case.
pub fn intrinsic_size(b: &[u8]) -> Option<(u32, u32)> {
    let n = natural_size(b)?;
    n.width.zip(n.height)
}

