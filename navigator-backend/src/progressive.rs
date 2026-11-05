//! ★★ PROGRESSIVE DISPLAY (legacy-web §5.4.5.7): a reader sees the tier-1
//! conversion as soon as it exists, and the scripted conversion replaces it
//! when it arrives — if it is worth replacing, and when the reader will not
//! see the seam.
//!
//! The order of events, for one session:
//!
//! 1. `OpenSession` with the FIRST recording (tier 1, no script run).
//! 2. `Upgrade` with the scripted conversion's READABLE recording. It is
//!    ingested, profile-checked and compared with the shown document NOW,
//!    off the reader's path. Not meaningfully different (`difference`): the
//!    first document stays, and the reader is never disturbed. Otherwise it
//!    is held, and the UI is told it is ready.
//! 3. `Swap` from the UI, at an idle moment of ITS choosing (no scroll, no
//!    input, nothing animating), carrying where the reader is: an `Anchor`,
//!    the element at the top of the viewport and how far into it. The new
//!    document replaces the old in place, and the answer is the same reading
//!    position in the new document (`map_anchor`): the UI composes the new
//!    scene scrolled there, and presents it in ONE frame — no blank frame
//!    between the two, no fade, no jump.
//! 4. `Attach` with the TRANSITIONS artifact, whenever exploration finishes.
//!    Bound by digest to the readable recording: attached at once if that is
//!    the shown document, kept for the swap if it is the held one, and — if
//!    it belongs to a scripted document that was KEPT OUT as not
//!    meaningfully different — that document becomes the held one, because
//!    a table recorded against one document must not be replayed on another.
//!    That swap shows no change, by the definition of "not meaningful".
//!
//! 0. (Between 1 and 2.) `Images`, as the first document's images arrive.
//!    The first document waited for its stylesheets and not its images:
//!    each `<img>` stood as a box sized from the page's own `width` and
//!    `height` (`pending: declared`), or with no natural size at all
//!    (`pending: placeholder`). Each images artifact is bound to the first
//!    document by digest and carries its whole images list so far
//!    (`FirstImages`). The document does not change; the session's recording
//!    does — it names the images that arrived, so the UI may ask for their
//!    pixels — and the UI is told which ones arrived and which ones changed
//!    size. None changed size: their pixels are painted into the boxes that
//!    were already there, and nothing moves. Some did: the UI lays the
//!    document out again at an idle moment, at the reader's `Anchor` (the
//!    same element, the same offset into it: on an unchanged document
//!    `map_anchor` is the identity), and presents it in one frame, as it
//!    presents a swap.
//!
//! Refused: a swap while the reader has changed the published document
//! (taken a step), whose state the swap would throw away; an upgrade for a
//! different URL; a second upgrade; a table for a document the session has
//! never held.

use crate::document::Document;
use crate::ingest::{ingest_transitions, Transitions};
use crate::session::{digest, Millis, SessionId, Sessions};
use crate::{ingest, Limits, Recording};
use navigator_dom::{Dom, Handle};
use std::collections::HashMap;

/// Fewer changed words than this is never a meaningful difference: a
/// date, a counter, a "Sign in" that became "Hi, Jo".
pub const MIN_WORDS: usize = 8;
/// …nor is a change under this fraction of the document's words, in 1/1000.
pub const MIN_PERMILLE: usize = 10;

/// How the scripted document differs from the shown one, as a reader would
/// notice: words (as a multiset — moved text is not new text) and images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub words_old: usize,
    pub words_new: usize,
    /// Words in one document and not the other, counted with multiplicity.
    pub words_changed: usize,
    /// Image sources in one document and not the other.
    pub images_changed: usize,
    pub meaningful: bool,
    /// Set when the numbers are not known here (`with_why`).
    pub said: Option<String>,
}

impl Difference {
    pub fn why(&self) -> String {
        if let Some(w) = &self.said { return w.clone() }
        format!("{} of {} words changed, {} images", self.words_changed, self.words_old.max(self.words_new), self.images_changed)
    }
    /// A difference known only by its sentence: what a jailed host hears
    /// from its worker, which made the comparison.
    pub fn with_why(mut self, why: &str) -> Self { self.said = Some(why.chars().take(200).collect()); self }
}

/// ★ "MEANINGFULLY DIFFERENT" IS WHAT A READER WOULD SEE: text that is not
/// there, or images that are not. Attribute churn (a `no-js` class that
/// became `js`), reordered markup and a handful of words are not, and a
/// reader is not made to watch a page re-lay itself out for them.
pub fn difference(old: &Dom, new: &Dom) -> Difference {
    let words = |d: &Dom| d.visible_text(d.root()).split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let (a, b) = (words(old), words(new));
    let mut m: HashMap<&str, i64> = HashMap::new();
    for w in &a { *m.entry(w).or_default() += 1 }
    for w in &b { *m.entry(w).or_default() -= 1 }
    let words_changed = m.values().map(|v| v.unsigned_abs() as usize).sum();
    let imgs = |d: &Dom| {
        let mut v: Vec<String> = d.by_tag_anywhere("img").into_iter()
            .filter(|&h| d.connected(h)).filter_map(|h| d.attr(h, "src").map(str::to_string)).collect();
        v.sort();
        v
    };
    let (ia, ib) = (imgs(old), imgs(new));
    let mut im: HashMap<&str, i64> = HashMap::new();
    for s in &ia { *im.entry(s).or_default() += 1 }
    for s in &ib { *im.entry(s).or_default() -= 1 }
    let images_changed = im.values().map(|v| v.unsigned_abs() as usize).sum();
    let total = a.len().max(b.len());
    let meaningful = images_changed > 0
        || (words_changed >= MIN_WORDS && words_changed * 1000 >= total * MIN_PERMILLE);
    Difference { words_old: a.len(), words_new: b.len(), words_changed, images_changed, meaningful, said: None }
}

/// Where a reader is: the element at the top of the viewport (its path, as
/// `Dom::exact_path` writes one) and how far below that element's top the
/// viewport starts, in NSG units (1/64 px).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub offset: i64,
}

/// How an anchor was carried into the new document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The anchor's own element is in the new document: same element, same
    /// offset into it.
    Same,
    /// It is not; the nearest element that is — in document order, then by
    /// ancestry — and the reader lands at its top.
    Nearest,
    /// Nothing near survived: the top of the document.
    Top,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub anchor: Anchor,
    pub how: How,
}

/// How far, in elements each way in document order, the nearest common
/// element is looked for before ancestors are tried.
pub const NEAR: usize = 64;

/// What makes an element the same element in two conversions of one page:
/// its `id`, else its tag with its OWN text (its text-node children, not its
/// subtree — a container whose descendants a script filled is still the
/// container) and the attributes that name content. An element with none of
/// those has no identity to match by.
fn key(d: &Dom, h: Handle) -> Option<String> {
    if let Some(id) = d.attr(h, "id").filter(|v| !v.is_empty()) { return Some(format!("#{id}")) }
    let tag = d.tag(h)?;
    let mut text = String::new();
    for c in d.children_of(h) {
        if d.node_type(c) == 3 {
            for w in d.text_content(c).split_whitespace() {
                if !text.is_empty() { text.push(' ') }
                text.push_str(w);
            }
        }
        if text.len() > 256 { break }
    }
    let attrs: Vec<String> = ["src", "href", "name", "alt"].iter()
        .filter_map(|a| d.attr(h, a).map(|v| format!("{a}={v}"))).collect();
    if text.is_empty() && attrs.is_empty() { return None }
    Some(format!("{tag}\u{1}{text}\u{1}{}", attrs.join("\u{1}")))
}

/// Connected elements in document order.
fn elements(d: &Dom) -> Vec<Handle> {
    let mut out = vec![];
    let mut stack = vec![d.root()];
    while let Some(h) = stack.pop() {
        if d.node_type(h) == 1 { out.push(h) }
        let kids = d.children_of(h);
        stack.extend(kids.into_iter().rev());
    }
    out
}

/// ★★ THE SAME READING POSITION IN THE NEW DOCUMENT. Scripts insert content
/// above where a reader is (a banner, a hydrated header), so keeping the
/// scroll OFFSET would move the text under them; keeping the ELEMENT does
/// not. The anchor's element is looked for in the new document by identity
/// (`key`); if it did not survive, the nearest one that did, in document
/// order and then by ancestry. A key shared by several elements matches by
/// its ordinal among them when both documents have the same number, and not
/// at all otherwise — a guess would put the reader somewhere else with no
/// sign it had.
pub fn map_anchor(old: &Dom, new: &Dom, a: &Anchor) -> Mapped {
    let top = Mapped { anchor: Anchor { path: String::new(), offset: 0 }, how: How::Top };
    let Some(e) = old.resolve_path(&a.path) else { return top };
    let (eo, en) = (elements(old), elements(new));
    let index = |d: &Dom, els: &[Handle]| {
        let mut m: HashMap<String, Vec<Handle>> = HashMap::new();
        for &h in els { if let Some(k) = key(d, h) { m.entry(k).or_default().push(h) } }
        m
    };
    let (io, inew) = (index(old, &eo), index(new, &en));
    let find = |h: Handle| -> Option<Handle> {
        let k = key(old, h)?;
        let (o, n) = (io.get(&k)?, inew.get(&k)?);
        if n.len() == 1 && o.len() == 1 { return Some(n[0]) }
        if n.len() == o.len() { return o.iter().position(|&x| x == h).map(|i| n[i]) }
        None
    };
    let found = |h: Handle, how: How, offset: i64| Mapped { anchor: Anchor { path: new.exact_path(h), offset }, how };
    if let Some(m) = find(e) { return found(m, How::Same, a.offset) }
    if let Some(pos) = eo.iter().position(|&h| h == e) {
        for d in 1..=NEAR {
            // ★ Above first: an element above the viewport's top edge keeps
            // the reader below where they were, never past text unread.
            for i in [pos.checked_sub(d), Some(pos + d)].into_iter().flatten() {
                if let Some(&h) = eo.get(i) { if let Some(m) = find(h) { return found(m, How::Nearest, 0) } }
            }
        }
    }
    let mut cur = e;
    while let Some(p) = old.get(cur).and_then(|n| n.parent) {
        if old.node_type(p) == 1 { if let Some(m) = find(p) { return found(m, How::Nearest, 0) } }
        cur = p;
    }
    top
}

/// A scripted recording the session has not shown.
#[derive(Debug, Clone)]
pub struct Staged {
    /// `blake3:<hex>` of `bytes`.
    pub digest: String,
    pub bytes: Vec<u8>,
    pub recording: Recording,
    /// Its transitions artifact, if that arrived before the swap.
    pub transitions: Option<Vec<u8>>,
}

/// What an upgrade decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upgrade {
    /// Not meaningfully different: the shown document stays.
    Kept(Difference),
    /// Held for the UI's next idle moment.
    Ready(Difference),
}

/// What happened to a transitions artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attached {
    /// On the shown document now.
    Now,
    /// It belongs to the held document, and goes in with it at the swap.
    WithSwap,
    /// It belongs to a scripted document that was kept out as not
    /// meaningfully different; that document is now held (`Ready`), since
    /// the table may only be replayed on the document it was recorded on.
    NeedsSwap,
}

/// What the swap put on screen, for the host's own bookkeeping (what the
/// session carries, what it stores).
#[derive(Debug)]
pub struct Swapped {
    pub mapped: Mapped,
    pub recording: Vec<u8>,
    pub transitions: Option<Vec<u8>>,
}

/// One session's progress from first display to the scripted document.
#[derive(Debug, Default)]
pub struct Progress {
    /// `blake3:<hex>` of the shown recording's bytes.
    pub shown: String,
    /// The first document's images, as they arrive.
    pub images: FirstImages,
    pub held: Option<Staged>,
    /// A scripted document judged not meaningfully different, kept in case
    /// its transitions arrive.
    pub kept_out: Option<Staged>,
    upgraded: bool,
}

pub fn address(bytes: &[u8]) -> String { format!("blake3:{}", digest(bytes)) }

impl Progress {
    pub fn opened(recording: &[u8]) -> Self {
        Progress { shown: address(recording), images: FirstImages::opened(recording), ..Default::default() }
    }

    pub fn upgrade(&mut self, sessions: &mut Sessions, id: SessionId, bytes: &[u8], limits: &Limits, now: Millis)
        -> Result<Upgrade, String>
    {
        if self.upgraded { return Err("this session has already had its scripted conversion".into()) }
        let s = sessions.get_mut(id, now).ok_or("no such session")?;
        let r = ingest(bytes, limits).map_err(|e| e.to_string())?;
        if r.url != s.url { return Err(format!("the scripted conversion is of {:?}, not this session's {:?}", r.url, s.url)) }
        // ★ The profile check happens HERE, not at the swap: a document that
        // would be refused is refused while the reader is still reading,
        // not at the idle moment the UI chose to replace what they read.
        let doc = Document::accept(&r.document).map_err(|v| format!("outside the profile: {} violations", v.len()))?;
        let d = difference(&s.document().dom, &doc.dom);
        self.upgraded = true;
        let staged = Staged { digest: address(bytes), bytes: bytes.to_vec(), recording: r, transitions: None };
        if d.meaningful { self.held = Some(staged); Ok(Upgrade::Ready(d)) } else { self.kept_out = Some(staged); Ok(Upgrade::Kept(d)) }
    }

    pub fn swap(&mut self, sessions: &mut Sessions, id: SessionId, anchor: &Anchor, limits: &Limits, now: Millis)
        -> Result<Swapped, String>
    {
        let s = sessions.get_mut(id, now).ok_or("no such session")?;
        let Some(held) = self.held.take() else { return Err("there is no scripted document to swap in".into()) };
        if !s.at_published() {
            self.held = Some(held);
            return Err("the reader has changed this document; the swap waits until they are back at its start".into());
        }
        let new = Document::accept(&held.recording.document).map_err(|v| format!("outside the profile: {} violations", v.len()))?;
        let mapped = map_anchor(&s.document().dom, &new.dom, anchor);
        let mut r = held.recording.clone();
        if let Some(t) = &held.transitions {
            let t = ingest_transitions(t, limits).map_err(|e| e.to_string())?;
            r.transitions = t.transitions;
        }
        if let Err(e) = sessions.replace(id, &r, now) {
            self.held = Some(held);
            return Err(e.to_string());
        }
        self.shown = held.digest;
        // The first document is gone, and its images with it: the scripted
        // one names its own.
        self.images.retire();
        Ok(Swapped { mapped, recording: held.bytes, transitions: held.transitions })
    }

    pub fn attach(&mut self, sessions: &mut Sessions, id: SessionId, bytes: &[u8], limits: &Limits, now: Millis)
        -> Result<Attached, String>
    {
        let t: Transitions = ingest_transitions(bytes, limits).map_err(|e| e.to_string())?;
        if t.readable == self.shown {
            let s = sessions.get_mut(id, now).ok_or("no such session")?;
            s.attach_transitions(t.transitions)?;
            return Ok(Attached::Now);
        }
        if let Some(h) = self.held.as_mut().filter(|h| h.digest == t.readable) {
            if h.transitions.is_some() { return Err("the held document already has its transition table".into()) }
            h.transitions = Some(bytes.to_vec());
            return Ok(Attached::WithSwap);
        }
        if let Some(mut k) = self.kept_out.take_if(|k| k.digest == t.readable) {
            k.transitions = Some(bytes.to_vec());
            self.held = Some(k);
            return Ok(Attached::NeedsSwap);
        }
        Err("this transition table was recorded against a document this session does not have".into())
    }
}

/// ★ ONE RECORDING FROM THE TWO PARTS, for a store that keeps a session's
/// recording to restore it from (`Saved` names one address). The table
/// replaces the readable recording's empty one; a table with templates
/// makes it a `/4` recording, as the converter's own merge does. Not byte
/// for byte the converter's merge — the store only needs what `ingest`
/// reads back to be the same.
pub fn compose(readable: &[u8], transitions: &[u8]) -> Result<Vec<u8>, String> {
    use serde_json::Value;
    let mut r: Value = serde_json::from_slice(readable).map_err(|e| e.to_string())?;
    let t: Value = serde_json::from_slice(transitions).map_err(|e| e.to_string())?;
    let (Some(ro), Some(to)) = (r.as_object_mut(), t.as_object()) else { return Err("not objects".into()) };
    ro.insert("transitions".into(), to.get("transitions").cloned().unwrap_or(Value::Array(vec![])));
    if let Some(tm) = to.get("templates") {
        ro.insert("templates".into(), tm.clone());
        ro.insert("format".into(), Value::String(crate::FORMAT_V4.into()));
    }
    if let Some(m) = ro.get_mut("measurements").and_then(Value::as_object_mut) {
        for k in ["interactive_found", "transitions_dropped"] {
            if let Some(v) = to.get(k) { m.insert(k.into(), v.clone()); }
        }
    }
    serde_json::to_vec(&r).map_err(|e| e.to_string())
}

/// The most entries an images artifact may carry: a recording names each
/// `src` once, and the corpus's largest page names 452.
pub const MAX_IMAGES: usize = 4096;

/// One entry of a recording's `images` block, as far as the broker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    pub src: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub ratio: String,
    /// `blake3:<hex>`, or empty: the bytes are not in hand.
    pub address: String,
    /// `declared` or `placeholder`: not fetched yet.
    pub pending: Option<String>,
}

impl ImageEntry {
    /// What the image's box is sized from: two entries that agree on this
    /// lay out the same.
    fn sizing(&self) -> (Option<u64>, Option<u64>, &str) { (self.width, self.height, &self.ratio) }
}

fn bad(field: &'static str, why: &'static str) -> crate::Reject { crate::Reject::BadField { field, why } }

/// Read an `images` array, bounded and checked: every `src` a bounded
/// string and each once, every dimension `null` or a `u32`, every address
/// empty or `blake3:<64 hex>`, `pending` absent or one of its two words.
pub fn image_entries(v: Option<&serde_json::Value>, limits: &Limits) -> Result<Vec<ImageEntry>, crate::Reject> {
    use serde_json::Value;
    let Some(v) = v else { return Ok(vec![]) };
    let arr = v.as_array().ok_or(bad("images", "not an array"))?;
    if arr.len() > MAX_IMAGES { return Err(crate::Reject::TooLarge { what: "images", measured: arr.len(), allowed: MAX_IMAGES }) }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let o = e.as_object().ok_or(bad("images", "an entry is not an object"))?;
        let s = |k: &'static str| -> Result<Option<String>, crate::Reject> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(v)) if v.len() <= limits.max_string_bytes => Ok(Some(v.clone())),
                Some(Value::String(_)) => Err(crate::Reject::TooLarge { what: "image field", measured: 0, allowed: limits.max_string_bytes }),
                Some(_) => Err(bad("images", "a field is not a string")),
            }
        };
        let dim = |k: &'static str| -> Result<Option<u64>, crate::Reject> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v.as_u64().filter(|n| *n <= u32::MAX as u64).map(Some).ok_or(bad("images", "a dimension is not null or a u32")),
            }
        };
        let src = s("src")?.ok_or(bad("images", "an entry has no src"))?;
        if !seen.insert(src.clone()) { return Err(bad("images", "a src appears twice")) }
        let address = s("address")?.unwrap_or_default();
        if !address.is_empty() && crate::image::address_hex(&address).is_none() {
            return Err(bad("images", "an address is not blake3:<64 hex>"));
        }
        let pending = s("pending")?;
        if pending.as_deref().is_some_and(|p| p != "declared" && p != "placeholder") {
            return Err(bad("images", "pending is neither declared nor placeholder"));
        }
        if pending.is_some() && !address.is_empty() { return Err(bad("images", "a pending image names bytes")) }
        out.push(ImageEntry { src, width: dim("width")?, height: dim("height")?, ratio: s("ratio")?.unwrap_or_default(), address, pending });
    }
    Ok(out)
}

/// What one images artifact changed, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagesAttached {
    /// The recording the session now shows: the first document with these
    /// images in. What it carries, and what a store keeps.
    pub merged: Vec<u8>,
    /// Every image has been handled; no more images artifacts are taken.
    pub complete: bool,
    /// Image addresses the session's recording names now and did not
    /// before: the UI may ask for their pixels (`Request::Image`).
    pub arrived: Vec<String>,
    /// `src`s whose box may change: their natural size differs from the one
    /// the shown layout used, or the image is gone (it failed). Empty: no
    /// layout changes, and the arrived pixels fill boxes already there.
    pub resized: Vec<String>,
}

/// ★★ THE FIRST DOCUMENT'S IMAGES, AS THEY ARRIVE (legacy-web §5.4.5.7).
///
/// The first document names each image it has not fetched as `pending`.
/// An images artifact (`atrium-navigator-images/1`) is bound to that
/// document by `first`, the BLAKE3 of its bytes, and carries the WHOLE
/// images block so far, so each one is applied to the first document as it
/// was opened — never to the previous merge — and a lost one costs nothing.
/// The merged recording is ingested again, as any recording is.
///
/// Refused: an artifact for another document; one after the complete one;
/// one after the first document was swapped out; a complete one with an
/// image still pending; and an entry that is malformed (`image_entries`).
#[derive(Debug, Clone, Default)]
pub struct FirstImages {
    /// `blake3:<hex>` of the first document's bytes.
    first: String,
    /// Its bytes, kept only while an image is pending.
    bytes: Option<Vec<u8>>,
    /// The images as the session shows them now.
    shown: Vec<ImageEntry>,
    retired: bool,
}

impl FirstImages {
    pub fn opened(recording: &[u8]) -> Self {
        let shown = serde_json::from_slice::<serde_json::Value>(recording).ok()
            .and_then(|v| image_entries(v.get("images"), &Limits::default()).ok()).unwrap_or_default();
        let pending = shown.iter().any(|e| e.pending.is_some());
        FirstImages { first: address(recording), bytes: pending.then(|| recording.to_vec()), shown, retired: false }
    }

    /// The first document is no longer shown.
    pub fn retire(&mut self) { self.retired = true; self.bytes = None }

    /// Whether an image of the shown document is still pending.
    pub fn pending(&self) -> bool { self.bytes.is_some() }

    pub fn attach(&mut self, bytes: &[u8], limits: &Limits) -> Result<ImagesAttached, String> {
        use serde_json::Value;
        if bytes.len() > limits.max_total_bytes { return Err(format!("an images artifact of {} bytes is over the limit", bytes.len())) }
        let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
        let o = v.as_object().ok_or("an images artifact is not an object")?;
        if o.get("format").and_then(Value::as_str) != Some(crate::IMAGES_FORMAT) { return Err("not an images artifact".into()) }
        let first = o.get("first").and_then(Value::as_str).ok_or("an images artifact without its first document")?;
        let complete = o.get("complete").and_then(Value::as_bool).ok_or("an images artifact without `complete`")?;
        let entries = image_entries(o.get("images"), limits).map_err(|e| e.to_string())?;
        if self.retired { return Err("the first document has been replaced; its images are not this document's".into()) }
        if first != self.first { return Err("these images were recorded against a document this session does not show".into()) }
        let Some(original) = &self.bytes else { return Err("this document has all its images already".into()) };
        if complete && entries.iter().any(|e| e.pending.is_some()) { return Err("a complete images artifact with an image still pending".into()) }
        // The first document, with this block for its own.
        let mut r: Value = serde_json::from_slice(original).map_err(|e| e.to_string())?;
        r.as_object_mut().ok_or("the first document is not an object")?.insert("images".into(), o["images"].clone());
        let merged = serde_json::to_vec(&r).map_err(|e| e.to_string())?;
        crate::ingest(&merged, limits).map_err(|e| format!("the first document with its images: {e}"))?;
        let before: HashMap<&str, &ImageEntry> = self.shown.iter().map(|e| (e.src.as_str(), e)).collect();
        let after: HashMap<&str, &ImageEntry> = entries.iter().map(|e| (e.src.as_str(), e)).collect();
        let mut resized: Vec<String> = self.shown.iter()
            .filter(|e| after.get(e.src.as_str()).is_none_or(|n| n.sizing() != e.sizing()))
            .map(|e| e.src.clone()).collect();
        resized.extend(entries.iter().filter(|e| !before.contains_key(e.src.as_str())).map(|e| e.src.clone()));
        let had: std::collections::HashSet<&str> = self.shown.iter().map(|e| e.address.as_str()).collect();
        let mut once = std::collections::HashSet::new();
        let arrived: Vec<String> = entries.iter().filter(|e| !e.address.is_empty() && !had.contains(e.address.as_str()))
            .filter(|e| once.insert(e.address.as_str())).map(|e| e.address.clone()).collect();
        self.shown = entries;
        if complete { self.bytes = None }
        Ok(ImagesAttached { merged, complete, arrived, resized })
    }
}
