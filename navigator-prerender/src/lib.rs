pub mod dom;
pub mod parse;
pub mod engine;
pub mod boa_impl;
pub mod fetch;
pub mod selector;

use dom::Script;
use engine::{ScriptEngine, ScriptSource};
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
    pub listeners_fired: u32,
    pub module_retries: u32,
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
    let mut scripts: Vec<ScriptSource> = vec![];
    for s in dom::scripts_in_order(&dom) {
        match s {
            Script::Inline { text, module } => scripts.push(ScriptSource { text, module }),
            Script::External { href, module } => {
                ext_total += 1;
                match resolve(base, &href) {
                    None => { ext_fail += 1; fetch_errors.push(format!("unresolved src: {href}")); }
                    Some(abs) => match fetcher.get(&abs) {
                        Ok(body) => { ext_ok += 1; scripts.push(ScriptSource { text: body, module }); }
                        Err(e) => { ext_fail += 1; fetch_errors.push(e); }
                    },
                }
            }
        }
    }

    let mut rep = engine.run(&mut dom, &scripts);
    rep.errors.extend(fetch_errors);
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
        missing: rep.missing,
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
