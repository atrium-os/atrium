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

pub struct RunReport {
    pub scripts_run: usize,
    pub scripts_failed: usize,
    pub errors: Vec<String>,
}

pub trait ScriptEngine {
    /// Name, for the report — which engine produced this conversion.
    fn name(&self) -> &'static str;
    /// Run each script in document order against the DOM, mutating it.
    fn run(&mut self, dom: &mut Dom, scripts: &[String]) -> RunReport;
}
