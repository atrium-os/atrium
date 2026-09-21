pub mod artifact;
// ★ Re-exported rather than re-implemented: `crate::dom::…` and
// `crate::parse::…` resolve exactly as before, so extracting these cost no
// churn at the call sites — which is the point of doing it this way round.
pub use navigator_dom::{dom, parse, profile};
pub mod engine;
pub mod boa_impl;
pub mod fetch;
pub mod selector;
pub mod modmirror;

use dom::Script;
use engine::{ScriptEngine, ScriptSource};
use std::collections::HashSet;
use fetch::Fetcher;

pub struct Conversion {
    /// The TIER 2 artifact: the document after its scripts ran.
    pub html: String,
    /// ★ The TIER 1 artifact: the same document before any script ran.
    ///
    /// Kept because tier 2 is not a separate pipeline — it is tier 1's DOM
    /// plus whatever the scripts changed — so the fallback is already in
    /// hand and costs one extra serialization rather than a second parse.
    /// Holding it is what lets a POLICY choose per document instead of the
    /// converter deciding for everyone.
    pub html_tier1: String,
    pub engine: &'static str,
    pub elements_before: usize,
    /// Visible text (whitespace-collapsed) before and after scripts ran.
    pub text_before: usize,
    pub text_after: usize,
    /// ★ Set when the ORIGIN refused rather than served: a challenge page,
    /// not the site. `None` for an ordinary document.
    pub origin_refusal: Option<&'static str>,
    /// Document Profile v1 ceilings this document exceeds, if any.
    pub profile_violations: Vec<profile::Violation>,
    /// Removals refused by protected-subtree execution (experiment).
    pub removals_refused: u32,
    /// ★ What the page DOES when acted on, learned by running its JS as an
    /// oracle and discarding the result. Empty unless the engine was asked
    /// to explore.
    pub transitions: Vec<engine::Transition>,
    /// Interactive elements found — the denominator for `transitions`.
    pub interactive_found: u32,
    pub elements_after: usize,
    pub depth_after: usize,
    pub scripts_total: usize,
    pub external_total: usize,
    pub external_fetched: usize,
    pub external_failed: usize,
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub script_mutations: u64,
    pub errors: Vec<String>,
    pub missing: Vec<(String, u32)>,
    pub nulls: Vec<(String, u32)>,
    pub first_error: Option<String>,
    pub cause: Option<(String, String)>,
    pub verdict: Verdict,
    pub listeners_fired: u32,
    pub module_retries: u32,
    pub observers_registered: u32,
    pub mutation_records: u32,
    pub ce_upgrades: u32,
    pub history_writes: u32,
    pub history_refused: u32,
    pub events_dispatched: u32,
    pub event_listeners_run: u32,
    pub doc_writes: u32,
    pub doc_writes_refused: u32,
    pub injected_scripts_run: u32,
    pub injected_scripts_refused: u32,
    pub layout_reads: u32,
    pub timers_fired: u32,
    pub timers_dropped: u32,
    pub page_fetches: u32,
    pub page_fetch_failures: u32,
    pub page_blocked: u32,
    pub blocked_hosts: Vec<(String, u32)>,
    pub beacons_suppressed: u32,
}

/// Parse, run the page's scripts once, and snapshot the result.
///
/// `base` is the document's own URL, needed to resolve `src` attributes.
/// External scripts are pulled through the `Fetcher` seam and run in document
/// order alongside inline ones.
pub fn convert_with(
    html: &str,
    base: Option<&str>,
    engine: &mut dyn ScriptEngine,
    fetcher: &mut dyn Fetcher,
) -> Conversion {
    // The CLI's convenience switch for the experiment; callers that want it
    // deterministically should use `convert_with_opts` and pass the flag.
    let protect = std::env::var("PRERENDER_PROTECT").ok().as_deref() == Some("1");
    convert_with_opts(html, base, engine, fetcher, protect)
}

/// `convert_with`, with the protected-subtree experiment under explicit
/// control rather than an environment variable.
///
/// ★ It is a PARAMETER because two tests toggling one env var in parallel
/// raced, and each saw the other's setting — a test that reads global
/// process state is not isolated, however careful it looks.
pub fn convert_with_opts(
    html: &str,
    base: Option<&str>,
    engine: &mut dyn ScriptEngine,
    fetcher: &mut dyn Fetcher,
    protect_parser_nodes: bool,
) -> Conversion {
    let mut dom = parse::parse(html);
    let before = dom.element_count();
    // ★ THE QUESTION TIER 2 ACTUALLY HAS TO ANSWER is not "did the scripts
    // run" but "is the artifact better than the raw HTML". Text length
    // before and after is the cheapest honest proxy: a converter that runs
    // everything cleanly and produces no more content than the parser did is
    // not earning its cost.
    let text_before = dom.visible_text(dom.root()).split_whitespace()
        .map(str::len).sum::<usize>();
    let html_tier1 = dom.serialize();
    // ★ Checked on the PARSED document, before scripts run. A profile
    // violation is a property of what was served, not of what our conversion
    // made of it.
    let profile_violations = profile::check(&dom, html.len());
    let origin_refusal = detect_origin_refusal(&dom);
    // ★ Always recorded, because ANCHORING needs it too: a transition is
    // replayable against tier 1 only if its trigger came from the parser.
    // Protection is the separate, opt-in thing.
    dom.parser_nodes = dom.nodes.len() as dom::Handle;
    dom.protect_parser_nodes = protect_parser_nodes;

    let (mut ext_total, mut ext_ok, mut ext_fail) = (0, 0, 0);
    let mut fetch_errors: Vec<String> = vec![];
    // Mirror root for THIS conversion's module graph (see modmirror).
    //
    // ★ Per conversion, not per process: keyed only by pid, two conversions in
    // one process wrote the same mirrored paths and clobbered each other —
    // caught by two tests sharing a root, and it would equally affect any
    // caller converting more than one document in-process.
    static CONV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nth = CONV.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mirror_root = std::env::temp_dir()
        .join(format!("prerender-mods-{}-{nth}", std::process::id()));
    let _ = std::fs::create_dir_all(&mirror_root);
    let mirror_root = mirror_root.canonicalize().unwrap_or(mirror_root);
    let mut seen: HashSet<String> = HashSet::new();
    let mut scripts: Vec<ScriptSource> = vec![];
    for s in dom::scripts_in_order(&dom) {
        match s {
            Script::Inline { text, module, element } => {
                // An inline module is written into the document's own
                // directory in the mirror, so ITS relative imports resolve
                // exactly as the page intends.
                let path = if module {
                    base.and_then(|b| modmirror::mirror_path(&mirror_root, b))
                        .map(|p| p.with_file_name(format!("inline-{}.mjs", scripts.len())))
                        .and_then(|p| {
                            let _ = p.parent().map(std::fs::create_dir_all);
                            // pull in what the inline module imports
                            if let Some(b) = base {
                                if let Ok(bu) = url::Url::parse(b) {
                                    for spec in modmirror::scan_imports(&text) {
                                        if spec.starts_with('.') || spec.starts_with('/') {
                                            if let Ok(abs) = bu.join(&spec) {
                                                let _ = modmirror::mirror(fetcher, abs.as_str(),
                                                    &mirror_root, &mut seen, 1, &mut fetch_errors);
                                            }
                                        }
                                    }
                                }
                            }
                            // ★ A failed mirror write used to yield None
                            // silently, and the inline module then ran with
                            // no path — failing later for a reason that had
                            // nothing to do with the page. Recorded instead.
                            match std::fs::write(&p, &text) {
                                Ok(()) => Some(p),
                                Err(e) => {
                                    fetch_errors.push(format!(
                                        "module mirror write failed for {}: {e}", p.display()));
                                    None
                                }
                            }
                        })
                } else { None };
                scripts.push(ScriptSource { text, module, path, element: Some(element) });
            }
            Script::External { href, module, element } => {
                ext_total += 1;
                match resolve(base, &href) {
                    None => { ext_fail += 1; fetch_errors.push(format!("unresolved src: {href}")); }
                    Some(abs) => {
                        if module {
                            // A module and everything it imports, mirrored so
                            // Boa's loader can resolve relative specifiers.
                            match modmirror::mirror(fetcher, &abs, &mirror_root,
                                                    &mut seen, 0, &mut fetch_errors) {
                                Some(path) => {
                                    ext_ok += 1;
                                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                                    scripts.push(ScriptSource { text, module: true, path: Some(path), element: Some(element) });
                                }
                                None => ext_fail += 1,
                            }
                        } else {
                            match fetcher.get(&abs) {
                                Ok(body) => { ext_ok += 1; scripts.push(ScriptSource { text: body, module, path: None, element: Some(element) }); }
                                Err(e) => { ext_fail += 1; fetch_errors.push(e); }
                            }
                        }
                    }
                }
            }
        }
    }

    engine.set_module_root(&mirror_root);
    engine.set_base_url(base);
    let mut rep = engine.run(&mut dom, &scripts);
    // The fetch cache persists; the mirror is scratch for this conversion.
    let _ = std::fs::remove_dir_all(&mirror_root);
    rep.errors.extend(fetch_errors);
    // Computed before the struct consumes the fields it reads.
    let verdict = classify(rep.first_error.as_deref(), rep.cause.as_ref());
    Conversion {
        html: dom.serialize(),
        html_tier1,
        engine: engine.name(),
        elements_before: before,
        text_before,
        origin_refusal,
        profile_violations,
        text_after: dom.visible_text(dom.root()).split_whitespace()
            .map(str::len).sum::<usize>(),
        removals_refused: dom.removals_refused,
        transitions: rep.transitions,
        interactive_found: rep.interactive_found,
        elements_after: dom.element_count(),
        depth_after: dom.max_depth(),
        scripts_total: scripts.len(),
        external_total: ext_total,
        external_fetched: ext_ok,
        external_failed: ext_fail,
        scripts_run: rep.scripts_run,
        scripts_failed: rep.scripts_failed,
        script_mutations: dom.script_mutations,
        listeners_fired: rep.listeners_fired,
        module_retries: rep.module_retries,
        observers_registered: rep.observers_registered,
        mutation_records: rep.mutation_records,
        ce_upgrades: rep.ce_upgrades,
        history_writes: rep.history_writes,
        history_refused: rep.history_refused,
        events_dispatched: rep.events_dispatched,
        event_listeners_run: rep.event_listeners_run,
        doc_writes: rep.doc_writes,
        doc_writes_refused: rep.doc_writes_refused,
        injected_scripts_run: rep.injected_scripts_run,
        injected_scripts_refused: rep.injected_scripts_refused,
        layout_reads: rep.layout_reads,
        timers_fired: rep.timers_fired,
        timers_dropped: rep.timers_dropped,
        page_fetches: rep.page_fetches,
        page_fetch_failures: rep.page_fetch_failures,
        page_blocked: rep.page_blocked,
        blocked_hosts: rep.blocked_hosts,
        beacons_suppressed: rep.beacons_suppressed,
        verdict,
        missing: rep.missing,
        first_error: rep.first_error,
        cause: rep.cause,
        nulls: rep.nulls,
        errors: rep.errors,
    }
}

/// Resolve a `src` against the document base. Absolute URLs pass through;
/// protocol-relative and path-relative forms need the base, and without one
/// they are reported rather than guessed at.
fn resolve(base: Option<&str>, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let b = base?;
    let b = url::Url::parse(b).ok()?;
    b.join(href).ok().map(|u| u.to_string())
}

/// Back-compat: convert with no network and no base.
pub fn convert(html: &str, engine: &mut dyn ScriptEngine) -> Conversion {
    convert_with(html, None, engine, &mut fetch::NoNetwork)
}

/// What a document's conversion outcome tells us about OUR coverage.
///
/// ★ Only the FIRST failure is classified. Once a script has thrown, later
/// scripts may fail because the first one never defined what they use, so
/// counting every failure conflates one gap with its consequences.
///
/// ★★ This is a heuristic, not a browser diff. A true control runs the same
/// page in a real browser and compares; that is a much larger apparatus. The
/// label is therefore "likely", and the categories are chosen so the
/// uncertainty lands in `Unknown` rather than being hidden inside a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    /// Every script ran.
    #[default]
    Clean,
    /// The first failure is a gap in this converter.
    OurGap,
    /// The first failure is one a real browser would also hit — typically a
    /// site-wide bundle querying for elements absent from this page.
    BrowserToo,
    /// Cannot be attributed either way.
    Unknown,
}

fn classify(first: Option<&str>, cause: Option<&(String, String)>) -> Verdict {
    let Some(e) = first else { return Verdict::Clean };
    // An environment probe reporting that we are not a usable browser is, by
    // its own account, our gap.
    if e.contains("not supported in this browser") { return Verdict::OurGap; }
    // Unambiguously ours, whatever preceded them.
    if e.contains("is not defined")
        || e.contains("not a callable")
        || e.contains("SyntaxError")
        || e.contains("could not open file")
        || e.contains("bare module specifier")
        || e.contains("module pending")
    {
        return Verdict::OurGap;
    }
    // ★ For a null/undefined throw, the PROXIMATE CAUSE decides — the last
    // recorded event before it, not a document-wide tally. A tally let a miss
    // from an unrelated script outvote the real cause and misclassified a
    // genuine browser-too failure as ours.
    if e.contains("cannot convert 'null' or 'undefined'") {
        return match cause.map(|(k, _)| k.as_str()) {
            Some("no-match") => Verdict::BrowserToo,
            Some("missing") | Some("ours") => Verdict::OurGap,
            _ => Verdict::Unknown,
        };
    }
    Verdict::Unknown
}

// ── Measurement, and the policy that reads it ───────────────────────────
//
// ★ THE CONVERTER MEASURES; THE POLICY DECIDES. These are deliberately
// separate types. A converter that silently returned a different artifact
// than the one its scripts produced would be making a product judgement
// inside an instrument, and the judgement would be invisible to the caller
// and untestable on its own.

impl Conversion {
    /// Fraction of the document's visible text that survived its scripts.
    /// 1.0 is unchanged, above 1.0 means content was added, below means lost.
    /// A document with no text to begin with returns 1.0: nothing was lost.
    pub fn text_retained(&self) -> f64 {
        if self.text_before == 0 { return 1.0 }
        self.text_after as f64 / self.text_before as f64
    }
}

/// Which artifact a policy chose, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The pre-script document: content taken from the HTML alone.
    One,
    /// The prerendered document: scripts ran and their result was kept.
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierDecision {
    pub tier: Tier,
    /// Why this tier, in words meant for a report rather than a log.
    pub reason: &'static str,
}

/// ★ NEVER EMIT AN ARTIFACT WORSE THAN THE INPUT.
///
/// Running a page's scripts is not automatically an improvement. Measured
/// over 84 scripted documents: 19 gained visible text, 63 were unchanged, and
/// 2 came out WORSE — bbc.co.uk runs all 70 of its scripts clean and drops
/// from 2,825 visible words to 970, because it is a hydrating app whose first
/// act is to tear down server-rendered content and whose re-render never
/// arrives (its data is cross-origin and refused, its chunks come from
/// dynamic import). For a document like that, tier 1 is the better artifact.
///
/// This is a FLOOR, not a quality check. A page that replaces good content
/// with an equal volume of worse content passes it, and it is named as a
/// floor so it is not trusted as more.
#[derive(Debug, Clone, Copy)]
pub struct TierPolicy {
    /// Minimum fraction of visible text a conversion must retain to be kept.
    ///
    /// The corpus separates cleanly: the one catastrophic case retains 0.346
    /// and the only other loss retains 0.996 (38 characters, noise). Any
    /// value between those two behaves identically here, so this default is
    /// chosen with margin on both sides rather than tuned — and it is the
    /// number an operator should expect to set deliberately.
    pub min_text_retained: f64,
    /// Documents with less visible text than this are exempt: an app shell
    /// that is empty before and after has not lost anything, and a ratio
    /// computed over a handful of characters is noise.
    pub text_floor: usize,
}

impl Default for TierPolicy {
    fn default() -> Self {
        Self { min_text_retained: 0.80, text_floor: 200 }
    }
}

impl TierPolicy {
    pub fn decide(&self, c: &Conversion) -> TierDecision {
        if c.scripts_total == 0 {
            return TierDecision { tier: Tier::One, reason: "no scripts to run" };
        }
        if c.text_before < self.text_floor {
            return TierDecision {
                tier: Tier::Two,
                reason: "too little text to judge; nothing to protect",
            };
        }
        if c.text_retained() < self.min_text_retained {
            return TierDecision {
                tier: Tier::One,
                reason: "conversion removed reader-visible content",
            };
        }
        TierDecision { tier: Tier::Two, reason: "conversion retained the content" }
    }

    /// The artifact this policy would publish, with the decision that chose
    /// it. Returning both is deliberate: the choice must travel WITH the
    /// bytes, or a reader cannot tell which pipeline produced what they see.
    pub fn artifact<'a>(&self, c: &'a Conversion) -> (&'a str, TierDecision) {
        let d = self.decide(c);
        (match d.tier { Tier::One => &c.html_tier1, Tier::Two => &c.html }, d)
    }
}

pub use engine::{Effect, Transition};

/// ★ A CHALLENGE PAGE IS NOT THE SITE, and converting one produces an
/// artifact that looks like a successful conversion of a document reading
/// "Just a moment...". Spec §5.4.1b: surface the refusal instead.
///
/// ★★ THE RULE IS DELIBERATELY CONSERVATIVE, because the obvious version is
/// wrong: an article ABOUT CAPTCHAs contains the word "captcha", and a news
/// story about Cloudflare quotes its interstitial. So a marker alone never
/// decides. A challenge page is also EMPTY — it has no article behind it —
/// and requiring both means a real document can carry any of these phrases
/// without being mistaken for a wall.
fn detect_origin_refusal(dom: &dom::Dom) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("just a moment", "interstitial challenge"),
        ("cf-browser-verification", "interstitial challenge"),
        ("cf_chl_", "interstitial challenge"),
        ("attention required!", "interstitial challenge"),
        ("checking your browser before", "interstitial challenge"),
        ("access to this page has been denied", "origin denied access"),
        ("access denied", "origin denied access"),
        ("you have been blocked", "origin denied access"),
        ("are you a robot", "bot check"),
        ("verify you are human", "bot check"),
        ("enable javascript and cookies to continue", "bot check"),
    ];
    // The whole point: a document with real content is not a wall, whatever
    // words it happens to contain.
    let visible = dom.visible_text(dom.root());
    if visible.split_whitespace().map(str::len).sum::<usize>() >= 200 {
        return None;
    }
    let hay = visible.to_ascii_lowercase();
    let title = dom.by_tag("title").first().map(|&h| dom.text_content(h))
        .unwrap_or_default().to_ascii_lowercase();
    for (needle, reason) in MARKERS {
        if title.contains(needle) || hay.contains(needle) {
            return Some(reason);
        }
    }
    None
}
