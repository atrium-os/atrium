//! The engine seam.
//!
//! ★ Kept deliberately narrow (spec §11.4): the minimal DOM is the bulk of the
//! work and is engine-agnostic, so a narrow embedding surface lets this same
//! instrument be re-run on a different engine. That is what makes it double as
//! the evaluation harness for §11.8's trigger 1 — measuring a candidate engine
//! against OUR corpus rather than a published conformance score.
//!
//! Everything engine-specific lives behind this trait. Adding Nova means
//! implementing it once; nothing else in the crate changes.

use crate::dom::Dom;

/// One script to run, and how it must be parsed.
pub struct ScriptSource {
    pub text: String,
    /// Where this module lives in the mirror, if it is one. Boa resolves
    /// relative specifiers against it.
    pub path: Option<std::path::PathBuf>,
    /// Declared `type="module"`, or inferred after a classic parse failed on
    /// module-only syntax.
    pub module: bool,
    /// The `<script>` element this came from, for `document.currentScript`.
    pub element: Option<crate::dom::Handle>,
}

/// One interactive element and what exploration made of it — the
/// measurement legacy-web §5.4.5.6 asks for before §5.4.5 is built.
///
/// ★ OBSERVATION ONLY. Collecting this changes neither which elements are
/// probed nor what is recorded; the recording is byte-identical with or
/// without it (checked against the corpus).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Interactive {
    pub path: String,
    pub tag: String,
    /// `type` for an input/button, else empty.
    pub input_type: String,
    /// Every event type the page registered on this element, sorted.
    pub events: Vec<String>,
    /// A native control: form, input (not hidden), textarea, select,
    /// details, button, or contenteditable.
    pub native: bool,
    /// Has a listener exploration has a gesture for.
    pub explorable: bool,
    /// At least one probe ran on it.
    pub probed: bool,
    /// An explorable candidate no probe reached (cut by the bound).
    pub over_cap: bool,
    /// A probe produced a recorded transition.
    pub recorded: bool,
    pub truncated: bool,
    /// Same-origin GETs the probe attempted while the network was closed.
    pub net_get: u32,
    /// Requests refused for being cross-origin or not GET.
    pub net_blocked: u32,
    pub beacons: u32,
    /// Timers the probe scheduled, and how many more were still pending
    /// after its drain than before (time the probe could not see).
    pub timers_scheduled: u32,
    pub timers_left: i64,
    /// Repeating timers pending after the probe.
    pub intervals_left: i64,
    /// What the recorded transitions wrote, for the interference count.
    pub effect_targets: Vec<String>,
    /// Probed as a delegated TARGET: a descendant the probe dispatched at
    /// on behalf of an ancestor's or the document's listener.
    pub delegated: bool,
    /// A delegating root: a probe dispatched at one of its descendants on
    /// its behalf recorded a transition.
    pub via: bool,
    /// Not in the interactive universe: a delegated target with no handler
    /// that is not a native control, or the document/window pseudo-entry.
    pub outside: bool,
    /// Cut by the budget, but every handler it would have exercised was
    /// probed on another element (step 2's per-handler budget).
    pub represented: bool,
    /// The gestures probed on it (click, hover, tap, swipe, key:Enter, …).
    pub gestures: Vec<String>,
    /// A probe on it hit the loop-iteration limit (a handler that spins).
    pub aborted: bool,
    /// Not probed, but an instance of a verified template (§5.4.5.6c): its
    /// transition is its siblings', predicted and checked on a sample.
    pub templated: bool,
    /// Probed as a template's verification sample.
    pub sampled: bool,
    /// Navigations its probes attempted (location.assign/replace/reload),
    /// every one refused: an effect exploration cannot record.
    pub nav_refused: u32,
    /// The refused requests' `<reason> <host>`, for telling data from
    /// analytics in case 1.
    pub blocked_hosts: Vec<String>,
    /// Not probed, but every sibling sharing its handler was probed and
    /// changed nothing observable, and so did a sample of the unprobed ones:
    /// the handler is known to do nothing visible here (§5.4.5.6c).
    pub inert: bool,
}

/// What exploration spent, per document (legacy-web §5.4.5.6a step 2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExploreStats {
    /// Distinct handler keys that had a candidate.
    pub handlers: u32,
    /// Probes run (direct, discovery and delegated).
    pub probes: u32,
    /// Candidates left unprobed by the bounds.
    pub cut: u32,
    /// Budget rounds started (round r gives each handler its (r+1)th probe).
    pub rounds: u32,
    /// Wall time spent exploring, for choosing the bounds. Report only;
    /// never in an artifact.
    pub micros: u64,
    /// Probes ended by the loop-iteration limit.
    pub aborted: u32,
    /// Probes each handler got, in key order (an unbounded run gives the
    /// candidates per handler, which is what the bounds are chosen from).
    pub per_handler: Vec<u32>,
    /// Templates kept, the instances they PREDICTED (not probed), the
    /// verification probes run, and templates refused because a sample
    /// disagreed with its prediction.
    pub templates: u32,
    pub predicted: u32,
    pub verify_probes: u32,
    pub refused: u32,
    /// Cut candidates whose handler group is verified to do nothing
    /// observable (every probe of the group, and a sample, recorded nothing).
    pub inert: u32,
}

/// ★ Layout geometry (legacy-web §5.4.5.6e): what the converter's layouts
/// did. Observation only, never in an artifact (`ms` is wall time).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeoStats {
    /// Layouts performed, and layouts that failed (both count toward
    /// `geometry::MAX_LAYOUTS`).
    pub layouts: u32,
    pub failures: u32,
    pub first_failure: String,
    /// Geometry reads answered from a layout of the DOM as it was, and from
    /// an older one because the budget was spent.
    pub reads: u32,
    pub stale: u32,
    /// Style writes refused for copying a measured size back.
    pub writes_refused: u32,
    pub first_refused: String,
    /// Observer records delivered (the one initial record each).
    pub io_records: u32,
    pub ro_records: u32,
    /// Viewports the reader's sweep scrolled through.
    pub scroll_steps: u32,
    /// Wall time in layouts, total and the longest one.
    pub ms: f64,
    pub max_ms: f64,
}

#[derive(Default)]
pub struct RunReport {
    /// Layout geometry: see `GeoStats`.
    pub geo: GeoStats,
    /// Exploration's budget accounting (only when exploring).
    pub explore_stats: ExploreStats,
    /// Per-element exploration coverage (only when exploring).
    pub coverage: Vec<Interactive>,
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub errors: Vec<String>,
    /// Properties the script asked a host object for and did not get —
    /// the missing-API report, by name.
    pub missing: Vec<(String, u32)>,
    /// Lookups that found nothing, by API and argument — the class the
    /// missing-API report cannot see.
    pub nulls: Vec<(String, u32)>,
    /// The FIRST script failure, which is the only one that can be classified:
    /// everything after it may be a cascade from it.
    pub first_error: Option<String>,
    /// The last recorded event before the first failure: its proximate cause.
    /// ("missing"|"no-match"|"ours", name)
    pub cause: Option<(String, String)>,
    /// Lifecycle handlers actually invoked — registration is not the point.
    pub listeners_fired: u32,
    /// Scripts that parsed only after being retried as modules.
    pub module_retries: u32,
    /// Geometry observations registered and deliberately never delivered —
    /// the visible cost of performing no layout.
    pub observers_registered: u32,
    /// Mutation records actually DELIVERED to a MutationObserver callback.
    /// The geometry observers count registrations because they deliver
    /// nothing; this one counts deliveries because it does.
    pub mutation_records: u32,
    /// Custom elements actually UPGRADED — constructor run and
    /// connectedCallback fired, not merely registered.
    pub ce_upgrades: u32,
    /// Same-document history writes (pushState + replaceState) the page made.
    pub history_writes: u32,
    /// Navigations refused because they would leave the document.
    pub history_refused: u32,
    /// Events the page dispatched ITSELF (never a synthesised user event).
    pub events_dispatched: u32,
    /// Listener invocations those dispatches actually produced.
    pub event_listeners_run: u32,
    /// document.write calls applied at the running script's position.
    pub doc_writes: u32,
    /// document.write calls refused because they would have erased the document.
    pub doc_writes_refused: u32,
    /// Scripts the page INJECTED at runtime and this converter executed.
    pub injected_scripts_run: u32,
    /// Injected scripts refused because they load third-party code.
    /// ★ What the page DOES when a reader acts on it — recorded by running
    /// its JS as an ORACLE rather than as a producer. See `explore`.
    pub transitions: Vec<Transition>,
    /// Transitions shared by sibling elements, recorded once (§5.4.5.6c).
    pub templates: Vec<Template>,
    /// Interactive elements found, whether or not probing them changed
    /// anything: the denominator for the transitions above.
    pub interactive_found: u32,
    pub injected_scripts_refused: u32,
    /// Reads of a layout metric (answered from a layout since §5.4.5.6e;
    /// see `geo`).
    pub layout_reads: u32,
    /// Timer callbacks dispatched, and those still pending at the horizon.
    pub timers_fired: u32,
    pub timers_dropped: u32,
    /// Requests the PAGE made, as distinct from fetching its own code.
    pub page_fetches: u32,
    pub page_fetch_failures: u32,
    /// Requests refused by the same-origin-GET policy, and where they aimed.
    pub page_blocked: u32,
    pub blocked_hosts: Vec<(String, u32)>,
    pub beacons_suppressed: u32,
}

pub trait ScriptEngine {
    /// Name, for the report — which engine produced this conversion.
    fn name(&self) -> &'static str;
    /// Run each script in document order against the DOM, mutating it.
    /// Root of the mirrored module graph for this document.
    fn set_module_root(&mut self, _root: &std::path::Path) {}
    /// The document's own URL, for `location` and relative `URL` resolution.
    fn set_base_url(&mut self, _url: Option<&str>) {}
    fn run(&mut self, dom: &mut Dom, scripts: &[ScriptSource]) -> RunReport;
}

/// One observable state transition: a trigger, and what it did.
///
/// ★ Deliberately a DESCRIPTION, not a DOM. The recording is meant to be
/// replayed against the tier 1 document, so it names the element by a path
/// that survives re-parsing and summarises the effect. Carrying the mutated
/// subtree would make this a second artifact, and then we would be back to
/// publishing what the scripts produced.
/// One observable EFFECT of a transition, in a form that can be replayed
/// against the tier 1 document.
///
/// ★ The distinction between these is the whole practical point. A menu that
/// opens by toggling a class needs NO content captured — the content is
/// already in the tier 1 document, merely hidden — and the transition is one
/// attribute write. A panel that inserts new nodes has to carry them. Lumping
/// both into "something changed" would force every transition to ship a
/// subtree it mostly does not need.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// An attribute written on a node that exists in tier 1.
    Attribute { target: String, name: String, from: Option<String>, to: Option<String> },
    /// Nodes inserted under a node that exists in tier 1, as serialized HTML,
    /// with the position they occupy among that parent's children.
    ///
    /// ★ The index is what makes replay FAITHFUL rather than merely
    /// plausible. Without it a replay appends, so a page that inserted a row
    /// into the middle of a list rendered it at the end — a difference no
    /// error would ever report. It is measured against the AFTER tree, which
    /// is why removals are applied before insertions: by then the indices
    /// mean what they meant when they were recorded.
    Insert { parent: String, index: usize, html: String },
    /// A node that exists in tier 1 was removed.
    Remove { target: String },
    /// The effect was too large to record; the count says how much was
    /// dropped, so a truncated recording is never mistaken for a small one.
    Truncated { dropped: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// Path to the trigger, as `#id` or a positional `tag:n>tag:n` chain.
    pub trigger: String,
    /// The event that caused it.
    pub event: String,
    /// Whether the trigger exists in the TIER 1 document. A transition on an
    /// element the scripts themselves created cannot be replayed there, and
    /// saying so is more useful than dropping it.
    pub anchored: bool,
    pub elements_added: i64,
    pub text_delta: i64,
    pub attributes_changed: u32,
    /// What actually happened, replayable against the tier 1 document.
    pub effects: Vec<Effect>,
}

/// ★★ ONE HANDLER'S TRANSITION, RECORDED ONCE, OVER THE ELEMENTS THAT SHARE IT
/// (legacy-web §5.4.5.6c).
///
/// The effects are stored once. An effect's address is a literal path (the
/// same node for every instance: a shared overlay), `$t` (the instance's own
/// trigger: a class toggle on itself), or `$N` (the instance's slot N: tab i
/// shows panel i, item i toggles its own body). Each instance binds a trigger
/// and its slots; expanding it gives exactly the `Transition` a probe on that
/// element would have recorded.
///
/// ★ A template is a COMPRESSION OF THE RECORDING, not a new behaviour: it
/// expands into the existing §5 vocabulary (attribute, insert, remove), so
/// the profile is unchanged and a consumer that expands at ingest replays it
/// down the ordinary path. Instances the converter did not probe are
/// PREDICTED from the structure the probed ones share, and the template is
/// kept only if probing a sample of them (first, middle, last) reproduced the
/// prediction exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub event: String,
    pub effects: Vec<Effect>,
    pub instances: Vec<Instance>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub trigger: String,
    pub anchored: bool,
    pub slots: Vec<String>,
    /// Probed (an example or a verification sample) rather than predicted.
    /// Report only; never in an artifact.
    pub probed: bool,
}

impl Template {
    /// The address an effect names for one instance, or None for a slot the
    /// instance does not bind.
    pub fn resolve<'a>(addr: &'a str, i: &'a Instance) -> Option<&'a str> {
        if addr == "$t" { return Some(&i.trigger) }
        match addr.strip_prefix('$') {
            Some(n) => n.parse::<usize>().ok().and_then(|n| i.slots.get(n)).map(String::as_str),
            None => Some(addr),
        }
    }

    /// The transition one instance stands for.
    pub fn expand(&self, i: &Instance) -> Option<Transition> {
        let r = |a: &String| Self::resolve(a, i).map(str::to_string);
        let mut effects = vec![];
        for e in &self.effects {
            effects.push(match e {
                Effect::Attribute { target, name, from, to } =>
                    Effect::Attribute { target: r(target)?, name: name.clone(), from: from.clone(), to: to.clone() },
                Effect::Insert { parent, index, html } =>
                    Effect::Insert { parent: r(parent)?, index: *index, html: html.clone() },
                Effect::Remove { target } => Effect::Remove { target: r(target)? },
                Effect::Truncated { dropped } => Effect::Truncated { dropped: *dropped },
            });
        }
        Some(Transition { trigger: i.trigger.clone(), event: self.event.clone(), anchored: i.anchored,
            elements_added: 0, text_delta: 0, attributes_changed: 0, effects })
    }

    pub fn is_attribute_only(&self) -> bool {
        !self.effects.is_empty()
            && self.effects.iter().all(|e| matches!(e, Effect::Attribute { .. }))
    }
}

impl Transition {
    /// A transition whose every effect is an attribute write needs no content
    /// carried with it: whatever it reveals is already in the tier 1
    /// document. This is the cheap, wholly-expressible case.
    pub fn is_attribute_only(&self) -> bool {
        !self.effects.is_empty()
            && self.effects.iter().all(|e| matches!(e, Effect::Attribute { .. }))
    }
}
