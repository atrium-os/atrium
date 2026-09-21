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

pub struct RunReport {
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
    /// Interactive elements found, whether or not probing them changed
    /// anything: the denominator for the transitions above.
    pub interactive_found: u32,
    pub injected_scripts_refused: u32,
    /// Reads of a layout metric this converter cannot truthfully answer.
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
}
