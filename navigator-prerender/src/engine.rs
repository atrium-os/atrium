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
    /// Lifecycle handlers actually invoked — registration is not the point.
    pub listeners_fired: u32,
    /// Scripts that parsed only after being retried as modules.
    pub module_retries: u32,
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
