pub mod dom;
pub mod parse;
pub mod engine;
pub mod boa_impl;

use engine::ScriptEngine;

pub struct Conversion {
    pub html: String,
    pub engine: &'static str,
    pub elements_before: usize,
    pub elements_after: usize,
    pub depth_after: usize,
    pub scripts_total: usize,
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub script_mutations: u64,
    pub errors: Vec<String>,
}

/// Parse, run the page's inline scripts once, and snapshot the result.
pub fn convert(html: &str, engine: &mut dyn ScriptEngine) -> Conversion {
    let mut dom = parse::parse(html);
    let before = dom.element_count();
    let scripts = dom::inline_scripts(&dom);
    let rep = engine.run(&mut dom, &scripts);
    Conversion {
        html: dom.serialize(),
        engine: engine.name(),
        elements_before: before,
        elements_after: dom.element_count(),
        depth_after: dom.max_depth(),
        scripts_total: scripts.len(),
        scripts_run: rep.scripts_run,
        scripts_failed: rep.scripts_failed,
        script_mutations: dom.script_mutations,
        errors: rep.errors,
    }
}
