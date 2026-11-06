//! Parsing the document a recording carries.
//!
//! ★ Deliberately a SECOND stage, after ingest. Ingest answers "is this a
//! recording"; this answers "what document does it carry". A caller that
//! cannot tell those apart cannot tell a malformed recording from a
//! well-formed one containing an unreasonable document.

use navigator_dom::{parse, profile, Dom};

pub use navigator_dom::profile::Violation;

/// The document, parsed, plus what the transitions can actually address.
#[derive(Debug)]
pub struct Document {
    /// ★ Changed only through `apply` (in place or by copy): the kept
    /// measures below (`stats`, `bytes`) and the compiled addresses follow
    /// those writes, not edits made to the tree from outside.
    pub dom: Dom,
    /// ★ What the profile ceilings measure, kept across in-place steps
    /// (`apply::apply_in_place_with_undo`): the element count exactly, and
    /// UPPER BOUNDS on serialized bytes, depth and stylesheets. A step proven
    /// inside every ceiling by these skips the whole-tree re-check; one that
    /// is not takes the exact path, which refreshes them. `None` until first
    /// needed.
    pub(crate) stats: Option<Stats>,
    /// ★ THE COMPILED FORM OF THIS STATE'S ADDRESSES (backend §4.3, replay
    /// part 1): every connected node holding an `id`, by id, in arena order —
    /// what `resolve_path("#id")` finds by scanning the whole arena, once per
    /// document state instead of once per address. Dropped by any write that
    /// can change what a path means (a structural change, or an `id`
    /// written), and rebuilt on next use; only the apply paths write.
    pub(crate) ids: std::cell::RefCell<Option<std::collections::HashMap<String, Vec<navigator_dom::Handle>>>>,
    /// ★ POSITIONAL ADDRESSES, compiled the same way (replay part 1b): each
    /// `html:0>body:0>…` path a step has resolved, with its answer (also
    /// "none"). A walk of the tree once per document state and path instead
    /// of once per click. Dropped with `ids`, by the same writes.
    pub(crate) paths: std::cell::RefCell<std::collections::HashMap<String, Option<navigator_dom::Handle>>>,
    /// ★ INSERTED MARKUP, parsed once: each `insert` effect's HTML, by its
    /// text, as the shared parser makes it. Parsing is a pure function of
    /// the text, so nothing ever invalidates it; the step grafts from the
    /// parsed tree instead of parsing at every click.
    pub(crate) frags: std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<Dom>>>,
    /// ★ The document's EXACT serialized length, once measured, kept by
    /// every in-place step from what it changed (`apply`): the attributes it
    /// wrote, the subtrees it removed and inserted, each measured by the
    /// serializer's own code. What a session is charged for its current
    /// document; serializing the whole page per click to learn it was 99%
    /// of a click's cost.
    pub(crate) bytes: std::cell::Cell<Option<usize>>,
    /// ★ WHAT CHANGED, AND SINCE WHEN (replay part 2): every step this
    /// document took in place, as the attributes it wrote and whether it
    /// changed the tree's shape, under a version that counts them. The
    /// display path reads it to tell a step nothing it draws depends on
    /// from one it must lay out again (`display::Bands::update`).
    pub(crate) journal: Journal,
    /// Which document this is: a document that took steps keeps it; one
    /// made anew (a swap, a restore) has its own, so the display path never
    /// mistakes it for a later state of another.
    pub(crate) lineage: u64,
}

/// See `Document::journal`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Journal { version: u64, log: std::collections::VecDeque<(u64, Touched)> }

/// What steps wrote: attribute names, lowercased, and whether any step
/// inserted or removed nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Touched {
    pub structure: bool,
    pub attrs: std::collections::BTreeSet<String>,
    /// An attribute was written inside an `<svg>` (the normalizer
    /// serializes an inline SVG whole and addresses it by hash, so no
    /// attribute there is unread).
    pub in_svg: bool,
}

/// Steps remembered (a reader further behind than this is shown the
/// document afresh).
const JOURNAL_STEPS: usize = 256;

impl Journal {
    pub(crate) fn record(&mut self, effects: &[crate::Effect], in_svg: bool) {
        let mut t = Touched { in_svg, ..Default::default() };
        for e in effects {
            match e {
                crate::Effect::Attribute { name, .. } => { t.attrs.insert(name.to_ascii_lowercase()); }
                _ => t.structure = true,
            }
        }
        self.version += 1;
        self.log.push_back((self.version, t));
        while self.log.len() > JOURNAL_STEPS { self.log.pop_front(); }
    }
}

static LINEAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// See `Document::stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats { pub elements: usize, pub bytes_ub: usize, pub depth_ub: usize, pub sheets_ub: usize }

impl Stats {
    pub(crate) fn exact(dom: &Dom) -> Stats {
        Stats { elements: dom.element_count(), bytes_ub: dom.serialized_len(), depth_ub: dom.max_depth(),
                sheets_ub: profile::stylesheet_count(dom) }
    }
}

impl Document {
    /// ★ The same parser the converter used — that is the whole reason
    /// `navigator-dom` exists. Re-parsing with a different one would let the
    /// producer and the renderer disagree about the same bytes.
    ///
    /// This does NOT check the profile. Callers handling a recording from the
    /// store want `accept`; this stays for tests and for tools that need to
    /// look at a document precisely because it is out of bounds.
    pub(crate) fn from_dom(dom: Dom) -> Self {
        Document { dom, stats: None, ids: Default::default(), paths: Default::default(), frags: Default::default(), bytes: Default::default(),
                   journal: Journal::default(), lineage: LINEAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }
    }

    /// How many steps this document has taken in place.
    pub fn version(&self) -> u64 { self.journal.version }

    /// Which document this is (see `lineage`).
    pub fn lineage(&self) -> u64 { self.lineage }

    /// What the steps after `version` wrote, together; `None` when they are
    /// no longer remembered (or `version` is not one of this document's).
    pub fn touched_since(&self, version: u64) -> Option<Touched> {
        let j = &self.journal;
        if version > j.version { return None }
        if version == j.version { return Some(Touched::default()) }
        let first = j.log.front().map(|e| e.0)?;
        if version + 1 < first { return None }
        let mut t = Touched::default();
        for (v, x) in &j.log {
            if *v <= version { continue }
            t.structure |= x.structure;
            t.in_svg |= x.in_svg;
            t.attrs.extend(x.attrs.iter().cloned());
        }
        Some(t)
    }

    /// `dom.serialized_len()`, measured once and then kept (see `bytes`).
    pub fn serialized_bytes(&self) -> usize {
        if let Some(b) = self.bytes.get() { return b }
        let b = self.dom.serialized_len();
        self.bytes.set(Some(b));
        b
    }

    /// An element's attributes as serialized: Σ ` name="value"`.
    pub(crate) fn attrs_len(&self, h: navigator_dom::Handle) -> usize {
        self.dom.get(h).map(|n| n.attrs.iter().map(|(k, v)| Dom::attr_serialized_len(k, v)).sum()).unwrap_or(0)
    }

    /// An insert's markup, parsed once (see `frags`).
    pub(crate) fn fragment(&self, html: &str) -> std::rc::Rc<Dom> {
        if let Some(f) = self.frags.borrow().get(html) { return f.clone() }
        let f = std::rc::Rc::new(navigator_dom::parse_fragment(html));
        self.frags.borrow_mut().insert(html.to_string(), f.clone());
        f
    }

    pub fn parse(html: &str) -> Self {
        Document::from_dom(parse(html))
    }

    /// ★★ THE PROFILE IS A RENDERER'S REFUSAL, NOT A CONVERTER'S WARNING.
    ///
    /// The converter reports violations and keeps going: it still holds the
    /// document, and saying *which* ceiling a real page broke is how the
    /// ceilings got corrected in the first place. The backend cannot afford
    /// that posture. Guarantee G3 is boundedness, and a document outside the
    /// profile is precisely the one for which no bound was ever established —
    /// so here a violation is a refusal.
    ///
    /// Both sides read the ceilings from `navigator_dom::profile`, for the
    /// same reason they read the tree from one parser. A renderer refusing
    /// documents its own converter happily emits is not a safety property, it
    /// is a broken pipeline.
    ///
    /// Note the ORDER: bytes are bounded at ingest, before this ever runs, so
    /// the parse below is over input of already-known size. Checking element
    /// and depth ceilings requires a tree, and building a tree from unbounded
    /// bytes to find out whether the bytes were bounded would be the check
    /// defeating itself.
    pub fn accept(html: &str) -> Result<Self, Vec<Violation>> {
        let dom = parse(html);
        let v = profile::check(&dom, html.len());
        if v.is_empty() { Ok(Document::from_dom(dom)) } else { Err(v) }
    }

    /// Resolve a transition's trigger against this document.
    ///
    /// ★★ A TRIGGER IS A CLAIM ABOUT THIS DOCUMENT, and this is where it
    /// stops being one. The recording says `#menu-toggle`; whether that
    /// element exists here is a question only the parsed document can answer,
    /// and a recording that names elements which are not present is either
    /// stale, tampered with, or was recorded against a different tier — all
    /// of which the caller needs to know BEFORE it starts applying effects.
    pub fn resolve(&self, trigger: &str) -> Option<navigator_dom::Handle> {
        // ★ `navigator_dom::Dom::resolve_path` is the inverse of the
        // `node_path` the converter wrote the address with — one function on
        // each side of the same crate. This used to be a walk of its own that
        // matched element tags only, so a text node's `node:N` never
        // resolved (chinadaily). Positional paths survive re-parsing only if
        // the document is byte-identical, which is true for tier 2: it is the
        // document the paths were recorded against.
        match trigger.strip_prefix('#') {
            // Exactly `resolve_path`'s answer: the one connected node holding
            // that id, or nothing if none or several do.
            Some(id) => match self.ids_index().get(id) { Some(v) if v.len() == 1 => Some(v[0]), _ => None },
            None => {
                if let Some(h) = self.paths.borrow().get(trigger) { return *h }
                let h = self.dom.resolve_path(trigger);
                self.paths.borrow_mut().insert(trigger.to_string(), h);
                h
            }
        }
    }

    fn ids_index(&self) -> std::cell::Ref<'_, std::collections::HashMap<String, Vec<navigator_dom::Handle>>> {
        if self.ids.borrow().is_none() {
            let mut m: std::collections::HashMap<String, Vec<navigator_dom::Handle>> = Default::default();
            for h in 0..self.dom.nodes.len() as navigator_dom::Handle {
                if let Some(id) = self.dom.attr(h, "id") {
                    if self.dom.connected(h) { m.entry(id.to_string()).or_default().push(h) }
                }
            }
            *self.ids.borrow_mut() = Some(m);
        }
        std::cell::Ref::map(self.ids.borrow(), |o| o.as_ref().expect("built"))
    }

    /// `Dom::exact_path`, with the id step checked through the index.
    pub fn exact_path(&self, h: navigator_dom::Handle) -> String {
        let p = self.dom.node_path(h);
        if self.resolve(&p) == Some(h) { p } else { self.dom.positional_path(h) }
    }

    /// Forget the compiled addresses: the tree changed in a way that can
    /// change what a path names.
    pub(crate) fn invalidate_addresses(&self) { *self.ids.borrow_mut() = None; self.paths.borrow_mut().clear() }
}
