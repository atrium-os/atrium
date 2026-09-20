pub mod dom;
pub mod parse;
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
    pub html: String,
    pub engine: &'static str,
    pub elements_before: usize,
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
    let mut dom = parse::parse(html);
    let before = dom.element_count();

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
                            std::fs::write(&p, &text).ok().map(|_| p)
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
        engine: engine.name(),
        elements_before: before,
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
