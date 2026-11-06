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
    let env = Env::default();
    let limit = limit_px as i64 * 64;
    with_fonts(|fonts| {
        // ★ The recording is PARSED only when this worker normalizes: a
        // stored normalized document needs nothing from it but the image
        // sizes, which `subresources_from_recording_json` reads line by line
        // without building the recording (the HTML standard's is 16 MiB of
        // JSON; parsing it was most of the cached arm's peak, §4.8).
        let (normalized, produced) = match (current, cached) {
            (None, Some(c)) => (String::from_utf8(c.to_vec()).map_err(|_| "the cached normalized document is not UTF-8")?, false),
            (None, None) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, &env).0, true)
            }
            (Some(doc), _) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(doc, &r.inputs, fonts, &env).0, false)
            }
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

// ---------------------------------------------------------------------------
// ★ BANDS OVER THE WIRE (pergola.md §10.4; backend §4.8): the worker holds a
// session's content source and answers `want` with the source's events and
// the groups it wrote, as NSG.
// ---------------------------------------------------------------------------

use navigator_render::source::{self, ContentSource, Event as SourceEvent, GroupId, Want};

/// What a view asks a session's content source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandsAsk {
    /// Keep `range` ready (1/64 px of the layout at `zoom_64`), with
    /// `prefetch` more either side.
    Want { range: (i64, i64), zoom_64: u32, prefetch: i64 },
    /// Background work: lay out up to `bands` more of the document.
    Idle { bands: u32 },
}

/// Bounds on an ask, checked by the broker before a worker sees it and by
/// the worker again.
pub const MAX_COORD: i64 = 1 << 40;
pub const MAX_IDLE_BANDS: u32 = 64;

impl BandsAsk {
    pub fn check(&self) -> Result<(), String> {
        match *self {
            BandsAsk::Want { range: (a, b), zoom_64, prefetch } => {
                if !(-MAX_COORD..=MAX_COORD).contains(&a) || !(-MAX_COORD..=MAX_COORD).contains(&b) || b < a { return Err(format!("range {a}..{b} is out of bounds")) }
                if !(crate::navigatord::MIN_ZOOM_64..=crate::navigatord::MAX_ZOOM_64).contains(&zoom_64) { return Err(format!("zoom {zoom_64}/64 is out of bounds")) }
                if !(0..=MAX_COORD).contains(&prefetch) { return Err(format!("prefetch {prefetch} is out of bounds")) }
                Ok(())
            }
            BandsAsk::Idle { bands } => if bands == 0 || bands > MAX_IDLE_BANDS { Err(format!("idle of {bands} bands is out of bounds")) } else { Ok(()) },
        }
    }
}

/// A group a source wrote: which, the band's range it is clipped to, NSG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPut { pub group: GroupId, pub clip: Option<(i64, i64)>, pub nsg: String }

/// What a worker answers for an ask.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BandsAnswer {
    pub events: Vec<SourceEvent>,
    /// Groups written, in order (a later put of a group replaces an earlier).
    pub puts: Vec<GroupPut>,
    pub clears: Vec<GroupId>,
    /// The normalized document, when this worker produced it for the
    /// recording's untouched initial state (for the broker to store).
    pub normalized: Option<Vec<u8>>,
}

/// A session's content source, in its worker: built on the first ask from
/// the normalized document (the stored one when the broker has it), and
/// brought up to date when the document changes (`update`, replay part 2).
pub struct Bands {
    src: source::DocumentSource<'static>,
    pub generation: u64,
    /// The recording's normalizer inputs, parsed when first needed.
    inputs: Option<std::rc::Rc<navigator_normalize::Inputs>>,
    /// The recording's image sizes, for sources built ahead.
    subs: navigator_render::Subresources,
    /// The document state the source shows: (lineage, version, state key).
    /// `None` until a session's document is first shown.
    shown: Option<(u64, u64, u64)>,
    /// ★ LAID-OUT STATES, by state key (`Session::state_key`): the one just
    /// left (so going back is a hit) and those the offerable triggers lead
    /// to (`precompute`, in idle time). Each is the normalized text and
    /// that text prepared at `zoom_64`. Most recently used last.
    cache: Vec<Cached>,
    /// Groups and events an update produced, sent with the next answer.
    pending: Option<NsgSink<'static>>,
    pending_events: Vec<SourceEvent>,
    /// The CSS text the inert test looks in, lowercased, for the document
    /// state (lineage, version) it was read from.
    css: Option<(u64, u64, String)>,
    /// ★ The states known to lay out exactly as the one shown (reached by
    /// steps the layout could not see): going to any of them is no work.
    same: Vec<u64>,
    /// How long this document's last normalize took, ms (`precompute`'s
    /// gate).
    normalize_ms: Option<f64>,
    pub counts: UpdateCounts,
}

/// A laid-out state, under every key known to reach it.
struct Cached { keys: Vec<u64>, zoom_64: u32, dark: bool, state: State }

/// What is kept of a state: its normalized text prepared (shown by laying
/// out again from where it differs, `DocumentSource::replace`), or a whole
/// source laid out for the view (shown by swapping it in, `swap_in`).
enum State { Prepared(String, navigator_render::html::Prepared), Source(source::DocumentSource<'static>) }

impl State {
    /// ★ What it holds in memory, ESTIMATED from its normalized text: a
    /// state prepared and laid out for a screen measured up to 24 times its
    /// text (HN thread: 106 MB for 4.5 MB; War and Peace 7 MB for 3.9 MB;
    /// `examples/state_memory`). The text alone would let six big states
    /// in — over 600 MB.
    fn bytes(&self) -> usize { 24 * match self { State::Prepared(h, _) => h.len(), State::Source(s) => s.html_len() } }
}

/// How each document change was shown (replay part 2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCounts {
    /// Proven to change nothing drawn: no restyle, no relayout.
    pub inert: u64,
    /// To a state already known to lay out as the one shown.
    pub same: u64,
    /// The new state was laid out already (cached): only the relayout.
    pub hit: u64,
    /// Normalized and prepared at the click, then the relayout.
    pub miss: u64,
    /// A different document (a swap): built afresh by the caller.
    pub rebuilt: u64,
    /// States normalized and prepared ahead, in idle time.
    pub precomputed: u64,
}

/// How many laid-out states a worker keeps, and the memory they may hold
/// together (`State::bytes`, an estimate): six of a corpus page's, two of
/// the HN thread's. A document whose one state is over it keeps none ahead
/// (its clicks are cold) but still keeps the one just left.
pub const CACHE_STATES: usize = 6;

/// The longest normalize of a document whose next states are built ahead in
/// idle time (`Bands::precompute`), ms.
pub const PRECOMPUTE_MS_MAX: f64 = 1000.0;
pub const CACHE_BYTES: usize = 256 << 20;

/// The pinned font set for a source that lives as long as its worker:
/// `FontSet::shared`, loaded once per process.
fn static_fonts() -> Result<&'static FontSet, String> { FontSet::shared() }

struct NsgSink<'a> { fonts: &'a FontSet, puts: Vec<GroupPut>, clears: Vec<GroupId> }

impl source::SceneSink for NsgSink<'_> {
    fn put(&mut self, group: GroupId, clip: Option<(i64, i64)>, scene: &navigator_render::Scene) {
        self.clears.retain(|g| *g != group);
        self.puts.retain(|p| p.group != group);
        self.puts.push(GroupPut { group, clip, nsg: navigator_render::nsg::write(scene, self.fonts) });
    }
    fn clear(&mut self, group: GroupId) {
        self.puts.retain(|p| p.group != group);
        if !self.clears.contains(&group) { self.clears.push(group) }
    }
}

/// ★ ATTRIBUTES NOTHING DRAWN READS, unless a stylesheet names them:
/// neither the normalizer nor the renderer reads `tabindex`, `aria-*` or
/// `data-*` by name (only a selector can: `[aria-expanded=true]`). A step
/// that wrote only these, outside any `<svg>`, to a document none of whose
/// CSS mentions them, changes nothing a reader sees — proven, so it is not
/// normalized, styled or laid out at all.
fn inert_name(n: &str) -> bool { n == "tabindex" || n.starts_with("aria-") || n.starts_with("data-") }

impl Bands {
    /// The source for a document: as `first_screen` chooses it (`recording`,
    /// `current`, `cached`).
    pub fn open(recording: &[u8], current: Option<&str>, cached: Option<&[u8]>, generation: u64) -> Result<(Bands, Option<Vec<u8>>), String> {
        let json = std::str::from_utf8(recording).map_err(|_| "the recording is not UTF-8")?;
        let env = Env::default();
        let fonts = static_fonts()?;
        let mut inputs = None;
        let t0 = std::time::Instant::now();
        let (normalized, produced) = match (current, cached) {
            (None, Some(c)) => (String::from_utf8(c.to_vec()).map_err(|_| "the cached normalized document is not UTF-8")?, false),
            (None, None) => {
                let r = navigator_normalize::recording::parse(json)?;
                let n = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, &env).0;
                inputs = Some(std::rc::Rc::new(r.inputs));
                (n, true)
            }
            (Some(doc), _) => {
                let r = navigator_normalize::recording::parse(json)?;
                let n = navigator_normalize::normalize_and_measure(doc, &r.inputs, fonts, &env).0;
                inputs = Some(std::rc::Rc::new(r.inputs));
                (n, false)
            }
        };
        let normalize_ms = inputs.is_some().then(|| t0.elapsed().as_secs_f64() * 1e3);
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let back = produced.then(|| normalized.clone().into_bytes());
        let src = source::DocumentSource::new(normalized, fonts, subs.clone(), (env.width_px, env.height_px), (env.height_px * 64.0) as i64);
        Ok((Bands { src, generation, inputs, subs, shown: None, cache: vec![], pending: None, pending_events: vec![], css: None, same: vec![], normalize_ms, counts: Default::default() }, back))
    }

    /// The recording's normalizer inputs (parsed once).
    fn inputs(&mut self, recording: &[u8]) -> Result<std::rc::Rc<navigator_normalize::Inputs>, String> {
        if let Some(i) = &self.inputs { return Ok(i.clone()) }
        let json = std::str::from_utf8(recording).map_err(|_| "the recording is not UTF-8")?;
        let i = std::rc::Rc::new(navigator_normalize::recording::parse(json)?.inputs);
        self.inputs = Some(i.clone());
        Ok(i)
    }

    /// Mark the source as showing `doc` in state `key` (a source just
    /// opened on the document as it is).
    pub fn showing(&mut self, doc: &crate::document::Document, key: u64) {
        self.shown = Some((doc.lineage(), doc.version(), key));
        self.same = vec![key];
    }

    /// The document state shown: (lineage, version, key).
    pub fn shown(&self) -> Option<(u64, u64, u64)> { self.shown }

    /// ★ THE DOCUMENT CHANGED (backend §4.3, replay part 2): bring the source
    /// to `doc`, in state `key`, doing only what the change requires:
    ///
    /// 1. steps that wrote only attributes nothing drawn reads
    ///    (`inert_name`, none named by the document's CSS) change nothing:
    ///    no normalizing, styling or layout;
    /// 2. otherwise the new state, normalized and prepared — from the cache
    ///    when a click was foreseen (`precompute`) or is a step back to the
    ///    state just left, else now — replaces the old one in the layout,
    ///    which is redone only from the checkpoint above the first change
    ///    and sends only the bands that changed (`DocumentSource::replace`).
    ///
    /// What it sends goes with the next answer. Returns which way it went.
    pub fn update(&mut self, recording: &[u8], doc: &crate::document::Document, key: u64, generation: u64) -> Result<UpdateKind, String> {
        self.generation = generation;
        let Some((lineage, version, shown_key)) = self.shown else { return Err("this source shows no session state".into()) };
        // A different document (a swap: another recording, other inputs)
        // is not a change of this one: its source is built afresh.
        if lineage != doc.lineage() { self.counts.rebuilt += 1; return Ok(UpdateKind::Rebuild) }
        if version == doc.version() && key == shown_key { return Ok(UpdateKind::Same) }
        if self.same.contains(&key) {
            self.shown = Some((lineage, doc.version(), key));
            self.counts.same += 1;
            return Ok(UpdateKind::Same);
        }
        let fonts = static_fonts()?;
        // 1. Proven to change nothing drawn.
        {
            if let Some(t) = doc.touched_since(version) {
                if !t.structure && !t.in_svg && !t.attrs.is_empty() && t.attrs.iter().all(|a| inert_name(a)) {
                    let css = self.css_of(recording, doc)?;
                    if t.attrs.iter().all(|a| !css.contains(a.as_str())) {
                        self.shown = Some((lineage, doc.version(), key));
                        self.same_push(key);
                        self.counts.inert += 1;
                        return Ok(UpdateKind::Inert);
                    }
                }
            }
        }
        // 2. The new state, laid out again from where it changed.
        let zoom = self.src.zoom_64();
        let found = self.cache.iter().position(|c| c.keys.contains(&key) && c.zoom_64 == zoom && c.dark == self.src.dark()).map(|i| self.cache.remove(i).state);
        let kind = if found.is_some() { UpdateKind::Hit } else { UpdateKind::Miss };
        let sink = self.pending.get_or_insert_with(|| NsgSink { fonts, puts: vec![], clears: vec![] });
        match found {
            // Laid out ahead: swapped in; the source it replaces is kept
            // whole (going back is a swap too).
            Some(State::Source(next)) => {
                let (ev, old) = self.src.swap_in(next, sink);
                self.pending_events.extend(ev);
                let keys = std::mem::replace(&mut self.same, vec![key]);
                self.keep(keys, zoom, State::Source(old));
                self.shown = Some((doc.lineage(), doc.version(), key));
                self.counts.hit += 1;
                return Ok(kind);
            }
            Some(State::Prepared(html, prep)) => {
                let ev = self.src.replace(html, Some(prep), sink);
                self.pending_events.extend(ev);
            }
            None => {
                let inputs = self.inputs(recording)?;
                let t0 = std::time::Instant::now();
                let html = navigator_normalize::normalize_and_measure(&doc.dom.serialize(), &inputs, fonts, &Env::default()).0;
                self.normalize_ms = Some(t0.elapsed().as_secs_f64() * 1e3);
                let sink = self.pending.get_or_insert_with(|| NsgSink { fonts, puts: vec![], clears: vec![] });
                let ev = self.src.replace(html, None, sink);
                self.pending_events.extend(ev);
            }
        }
        // The state left is kept, under every key known to reach it: going
        // back to it is a hit. A step the layout could not see joins the
        // states known to look the same.
        match self.src.take_previous() {
            Some((h, p)) => { let keys = std::mem::replace(&mut self.same, vec![key]); self.keep(keys, zoom, State::Prepared(h, p)) }
            None if self.src.replaced_from().is_none() => self.same_push(key),
            None => self.same = vec![key],
        }
        let _ = shown_key;
        self.shown = Some((doc.lineage(), doc.version(), key));
        match kind { UpdateKind::Hit => self.counts.hit += 1, _ => self.counts.miss += 1 }
        Ok(kind)
    }

    fn same_push(&mut self, key: u64) {
        if !self.same.contains(&key) { self.same.push(key) }
        if self.same.len() > 64 { self.same.remove(1); }
    }

    fn keep(&mut self, keys: Vec<u64>, zoom_64: u32, state: State) {
        let dark = self.src.dark();
        self.cache.retain(|c| c.zoom_64 != zoom_64 || c.dark != dark || !c.keys.iter().any(|k| keys.contains(k)));
        self.cache.push(Cached { keys, zoom_64, dark: self.src.dark(), state });
        while self.cache.len() > CACHE_STATES || self.cache.iter().map(|c| c.state.bytes()).sum::<usize>() > CACHE_BYTES {
            if self.cache.len() <= 1 { break }
            self.cache.remove(0);
        }
    }

    /// Where the element with this `id` begins, as the band holding it
    /// (measurement: a view placed where a reader sees a trigger).
    pub fn band_range_of_id(&self, id: &str) -> Option<(i64, i64)> {
        let b = self.src.banded();
        b.band_of_id(id).map(|k| b.bands[k].range)
    }

    /// Whether the whole document is laid out and final.
    pub fn settled(&self) -> bool { self.src.banded().settled() }

    /// Whether state `key` is laid out ahead (at the current zoom).
    pub fn cached(&self, key: u64) -> bool { let (z, d) = (self.src.zoom_64(), self.src.dark()); self.same.contains(&key) || self.cache.iter().any(|c| c.keys.contains(&key) && c.zoom_64 == z && c.dark == d) }

    /// ★ IN IDLE TIME, the state just left laid out for the view: kept as
    /// its prepared text by a click shown in place (`replace`), it becomes a
    /// whole source, so going back to it is a swap (`swap_in`) and not a
    /// relayout. Layout only — nothing is normalized. Returns whether there
    /// was one to do.
    pub fn prepare_back(&mut self) -> bool {
        let zoom = self.src.zoom_64();
        let dark = self.src.dark();
        let Some(i) = self.cache.iter().rposition(|c| c.zoom_64 == zoom && c.dark == dark && matches!(c.state, State::Prepared(..))) else { return false };
        // Only the most recent state left (the one `back` returns to).
        if i + 1 != self.cache.len() { return false }
        let Ok(fonts) = static_fonts() else { return false };
        let c = self.cache.remove(i);
        let State::Prepared(html, prep) = c.state else { unreachable!() };
        let d = Env::default();
        let mut s = source::DocumentSource::from_prepared(html, prep, fonts, self.subs.clone(), (d.width_px, d.height_px), (d.height_px * 64.0) as i64, zoom, dark);
        if let Some(w) = self.src.last_want() { s.prime(w) }
        self.cache.push(Cached { keys: c.keys, zoom_64: zoom, dark, state: State::Source(s) });
        true
    }

    /// ★ IN IDLE TIME, the states a click can reach from here (`next`: each
    /// offerable transition and the key it leads to), normalized and
    /// prepared ahead, so that click is only its relayout. At most `max`
    /// of them per call, nearest the table's start first; states already
    /// kept are skipped. Returns how many were laid out.
    pub fn precompute(&mut self, recording: &[u8], doc: &crate::document::Document, next: &[(&crate::Transition, u64)], max: usize) -> Result<usize, String> {
        let fonts = static_fonts()?;
        let (zoom, dark) = (self.src.zoom_64(), self.src.dark());
        let mut n = 0;
        // A state ahead must fit beside the one just left (kept for going
        // back), or it is not built at all: built and evicted at once, it
        // would cost a normalize on every idle ask.
        // ★ The worker answers one request at a time: a click arriving while
        // a state is built ahead waits for it. So only a document that
        // normalizes in under `PRECOMPUTE_MS_MAX` is built ahead (corpus
        // pages: p99 0.84 s; the big documents: 1.5–4 s, whose clicks stay
        // cold rather than risk waiting behind one).
        // (Not yet measured — opened from the stored normalized document —
        // a document of at most 1 MB normalized is taken to be quick; its
        // first build ahead measures it.)
        let quick = match self.normalize_ms { Some(ms) => ms <= PRECOMPUTE_MS_MAX, None => self.src.html_len() <= 1 << 20 };
        if !quick { return Ok(0) }
        let est = 24 * self.src.html_len();
        let back = self.cache.last().map(|c| c.state.bytes()).unwrap_or(0);
        if est + back > CACHE_BYTES { return Ok(0) }
        for (t, key) in next {
            if n >= max { break }
            if self.cached(*key) { continue }
            // Applied to a copy (the reader has not clicked it); a step that
            // would be refused is skipped.
            let Ok(after) = doc.applied(t) else { continue };
            let inputs = self.inputs(recording)?;
            let t0 = std::time::Instant::now();
            let html = navigator_normalize::normalize_and_measure(&after.dom.serialize(), &inputs, fonts, &Env::default()).0;
            self.normalize_ms = Some(t0.elapsed().as_secs_f64() * 1e3);
            let d = Env::default();
            let prep = navigator_render::html::prepare(&html, fonts, &Env { dark, ..Env::zoomed(d.width_px, d.height_px, zoom) });
            // Laid out for the view as it is now, so the click only swaps
            // it in (`DocumentSource::swap_in`).
            let mut next = source::DocumentSource::from_prepared(html, prep, fonts, self.subs.clone(), (d.width_px, d.height_px), (d.height_px * 64.0) as i64, zoom, dark);
            if let Some(w) = self.src.last_want() { next.prime(w) }
            // Kept ahead of what is used least recently, behind the state
            // just left (going back stays a hit).
            self.cache.retain(|c| !c.keys.contains(key) || c.zoom_64 != zoom || c.dark != dark);
            let at = self.cache.len().saturating_sub(1);
            self.cache.insert(at, Cached { keys: vec![*key], zoom_64: zoom, dark, state: State::Source(next) });
            // Older states go first; never this one (at `mine`) or the one
            // just left (after it).
            let mut mine = at;
            while mine > 0 && (self.cache.len() > CACHE_STATES || self.cache.iter().map(|c| c.state.bytes()).sum::<usize>() > CACHE_BYTES) {
                self.cache.remove(0);
                mine -= 1;
            }
            self.counts.precomputed += 1;
            n += 1;
        }
        Ok(n)
    }

    /// The CSS the inert test looks in: every supplied stylesheet and every
    /// `<style>` of the document as it is, lowercased, escapes dropped.
    fn css_of(&mut self, recording: &[u8], doc: &crate::document::Document) -> Result<&str, String> {
        let fresh = match &self.css {
            Some((l, v, _)) if *l == doc.lineage() => doc.touched_since(*v).is_some_and(|t| !t.structure),
            _ => false,
        };
        if !fresh {
            let inputs = self.inputs(recording)?;
            let mut text = String::new();
            for v in inputs.stylesheets.values() { text.push_str(v); text.push('\n') }
            for h in doc.dom.by_tag_anywhere("style") { text.push_str(&doc.dom.text_content(h)); text.push('\n') }
            let text = text.to_ascii_lowercase().replace('\\', "");
            self.css = Some((doc.lineage(), doc.version(), text));
        }
        Ok(&self.css.as_ref().expect("set").2)
    }

    /// Answer one ask (with whatever an update since the last one sent).
    pub fn ask(&mut self, a: BandsAsk) -> Result<BandsAnswer, String> {
        a.check()?;
        let mut sink = self.pending.take().unwrap_or(NsgSink { fonts: static_fonts()?, puts: vec![], clears: vec![] });
        let mut events = std::mem::take(&mut self.pending_events);
        events.extend(match a {
            BandsAsk::Want { range, zoom_64, prefetch } => self.src.want(Want { range, zoom_64, prefetch }, &mut sink),
            BandsAsk::Idle { bands } => self.src.idle(bands as usize, &mut sink),
        });
        Ok(BandsAnswer { events, puts: sink.puts, clears: sink.clears, normalized: None })
    }
}

/// How `Bands::update` showed a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind { Same, Inert, Hit, Miss, Rebuild }

/// ★ THE WIRE FORM of an answer (worker → broker), line based:
/// `<normalized_len>\n` and that many bytes; then one line per item —
/// `E extent <height> <0|1>`, `E ready|updated <band> <y0> <y1>`,
/// `E evicted <band>`, `C <group>`, `P <group> <y0|-> <y1|-> <len>` followed
/// by a newline and `len` bytes of NSG — where a group is `b<band>` or `o`.
pub fn encode_answer(a: &BandsAnswer) -> Vec<u8> {
    let g = |g: &GroupId| match g { GroupId::Band(b) => format!("b{b}"), GroupId::Overlay => "o".into() };
    let n = a.normalized.as_deref().unwrap_or(&[]);
    let mut out = format!("{}\n", n.len()).into_bytes();
    out.extend_from_slice(n);
    for e in &a.events {
        let line = match e {
            SourceEvent::Extent { height, settled } => format!("E extent {height} {}\n", *settled as u8),
            SourceEvent::BandReady { band, range } => format!("E ready {band} {} {}\n", range.0, range.1),
            SourceEvent::BandUpdated { band, range } => format!("E updated {band} {} {}\n", range.0, range.1),
            SourceEvent::BandEvicted { band } => format!("E evicted {band}\n"),
        };
        out.extend_from_slice(line.as_bytes());
    }
    for c in &a.clears { out.extend_from_slice(format!("C {}\n", g(c)).as_bytes()) }
    for p in &a.puts {
        let (y0, y1) = p.clip.map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or(("-".into(), "-".into()));
        out.extend_from_slice(format!("P {} {y0} {y1} {}\n", g(&p.group), p.nsg.len()).as_bytes());
        out.extend_from_slice(p.nsg.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Parse a worker's answer, strictly: every number is digits (with a sign
/// where a coordinate may be negative), every length within the payload,
/// and nothing else.
pub fn decode_answer(p: &[u8]) -> Result<BandsAnswer, String> {
    let mut at = 0usize;
    let line = |at: &mut usize| -> Result<&str, String> {
        let rest = &p[*at..];
        let nl = rest.iter().take(256).position(|&b| b == b'\n').ok_or("a line is missing or too long")?;
        let s = std::str::from_utf8(&rest[..nl]).map_err(|_| "a line is not text")?;
        *at += nl + 1;
        Ok(s)
    };
    let num = |v: &str| -> Result<i64, String> {
        let d = v.strip_prefix('-').unwrap_or(v);
        if d.is_empty() || d.len() > 19 || !d.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("{v:?} is not a number")) }
        v.parse().map_err(|_| format!("{v:?} is not a number"))
    };
    let unum = |v: &str| -> Result<u64, String> {
        if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("{v:?} is not a count")) }
        v.parse().map_err(|_| format!("{v:?} is not a count"))
    };
    let group = |v: &str| -> Result<GroupId, String> {
        if v == "o" { return Ok(GroupId::Overlay) }
        let b = v.strip_prefix('b').ok_or_else(|| format!("{v:?} is not a group"))?;
        Ok(GroupId::Band(u32::try_from(unum(b)?).map_err(|_| "band id out of range")?))
    };
    let mut a = BandsAnswer::default();
    let n = unum(line(&mut at)?)? as usize;
    if n > crate::navigatord::MAX_NORMALIZED_BYTES || at + n > p.len() { return Err("normalized length out of bounds".into()) }
    if n > 0 { a.normalized = Some(p[at..at + n].to_vec()) }
    at += n;
    while at < p.len() {
        let l = line(&mut at)?.to_string();
        let f: Vec<&str> = l.split(' ').collect();
        match f.as_slice() {
            ["E", "extent", h, s] => a.events.push(SourceEvent::Extent { height: num(h)?, settled: match *s { "0" => false, "1" => true, _ => return Err("settled is not 0 or 1".into()) } }),
            ["E", k @ ("ready" | "updated"), b, y0, y1] => {
                let band = u32::try_from(unum(b)?).map_err(|_| "band id out of range")?;
                let range = (num(y0)?, num(y1)?);
                a.events.push(if *k == "ready" { SourceEvent::BandReady { band, range } } else { SourceEvent::BandUpdated { band, range } });
            }
            ["E", "evicted", b] => a.events.push(SourceEvent::BandEvicted { band: u32::try_from(unum(b)?).map_err(|_| "band id out of range")? }),
            ["C", g] => a.clears.push(group(g)?),
            ["P", g, y0, y1, len] => {
                let clip = match (*y0, *y1) { ("-", "-") => None, (a0, b0) => Some((num(a0)?, num(b0)?)) };
                let len = unum(len)? as usize;
                if at + len + 1 > p.len() || p[at + len] != b'\n' { return Err("a group's length is out of bounds".into()) }
                let nsg = String::from_utf8(p[at..at + len].to_vec()).map_err(|_| "a group is not UTF-8")?;
                at += len + 1;
                a.puts.push(GroupPut { group: group(g)?, clip, nsg });
            }
            _ => return Err(format!("unknown line {:?}", l.chars().take(40).collect::<String>())),
        }
    }
    Ok(a)
}
