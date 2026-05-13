//! Differential test harness.
//!
//! Compiles a SPIR-V module through every registered
//! [`ShaderRunner`], executes each with the same inputs,
//! and asserts agreement.
//!
//! Disagreement-pattern → bug-location mapping (see
//! `docs/spec/tier2-renderer.md` §10.2):
//!
//! | bespoke | cranelift | interpreter | diagnosis                      |
//! |---------|-----------|-------------|--------------------------------|
//! | A       | A         | A           | shader works ✓                 |
//! | A       | A         | B           | frontend bug (both prod paths share frontend) |
//! | A       | B         | B           | bespoke backend bug            |
//! | B       | A         | B           | cranelift adapter bug          |
//! | A       | B         | C           | multiple bugs; investigate     |
//!
//! # Phase 0 status
//!
//! Only the interpreter is registered as a `ShaderRunner`
//! at this stage; the production backends will register
//! their own implementations once they land (phases 2 + 3).
//! Phase 0 tests therefore assert "interpreter agrees with
//! itself," which is trivially true but exercises the
//! harness plumbing.

use crate::interpreter::{Interpreter, ShaderInputs, ShaderOutputs};
use crate::pixels::{compare_buffers, ColorTolerance, PixelMismatch};

/// A backend that can compile + execute a SPIR-V module.
///
/// Each of the three production tiers (bespoke / Cranelift /
/// interpreter) implements this. The harness invokes
/// `run` on each registered runner with the same inputs
/// and compares outputs.
pub trait ShaderRunner: std::fmt::Debug {
    /// Identifier for this runner, used in failure messages
    /// (e.g. `"bespoke"`, `"cranelift"`, `"interpreter"`).
    fn name(&self) -> &'static str;

    /// Compile + execute the SPIR-V module with the given
    /// fragment-shader-style inputs. Returns the produced
    /// pixel buffer.
    ///
    /// Backends may return `Err(BackendError::Unsupported)`
    /// for shaders they can't compile; the harness will
    /// skip that runner for the failing shader rather than
    /// failing the assertion. (E.g. early bespoke can't
    /// handle most ops; Cranelift fills the gap.)
    fn run(
        &self,
        spirv: &[u8],
        inputs: &ShaderInputs,
    ) -> Result<ShaderOutputs, BackendError>;
}

/// Backend-side errors.
#[derive(Debug)]
pub enum BackendError {
    /// Shader uses opcodes / capabilities this backend
    /// doesn't (yet) handle. The harness skips this
    /// runner for this shader without failing the test.
    Unsupported(String),
    /// Backend failed to compile the shader. The harness
    /// fails the test — Unsupported is the legitimate
    /// "skip" signal; CompileFailed indicates the backend
    /// thought it should handle the shader but errored.
    CompileFailed(String),
    /// Shader compiled but execution failed (e.g.
    /// dlopen failure, runtime panic). Harness fails the
    /// test.
    Runtime(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unsupported(why) => write!(f, "unsupported: {why}"),
            BackendError::CompileFailed(why) => write!(f, "compile failed: {why}"),
            BackendError::Runtime(why) => write!(f, "runtime error: {why}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// A runner backed by the [`crate::interpreter::Interpreter`].
///
/// Always registered. Defined here (rather than in
/// `interpreter`) so the interpreter module stays free of
/// harness dependencies.
#[derive(Debug, Default)]
pub struct InterpreterRunner;

impl ShaderRunner for InterpreterRunner {
    fn name(&self) -> &'static str { "interpreter" }

    fn run(
        &self,
        spirv: &[u8],
        inputs: &ShaderInputs,
    ) -> Result<ShaderOutputs, BackendError> {
        let interp = Interpreter::new(spirv)
            .map_err(|e| BackendError::CompileFailed(format!("{e:?}")))?;
        interp.run_fragment(inputs)
            .map_err(|e| match e {
                crate::interpreter::InterpError::UnsupportedOpcode(op) =>
                    BackendError::Unsupported(format!("opcode {op}")),
                other =>
                    BackendError::Runtime(format!("{other:?}")),
            })
    }
}

/// The canonical differential-test entry point.
///
/// Compiles `spirv` through every passed-in runner,
/// executes each with `inputs`, and asserts pixel-
/// equivalent outputs within `tolerance`.
///
/// Runners that return [`BackendError::Unsupported`] for
/// the given shader are skipped (with a logged note);
/// runners that return any other error fail the assertion
/// outright.
///
/// On disagreement, panics with a structured message
/// naming the runners and the first divergent pixel,
/// suitable for `cargo test`'s default output.
///
/// # Panics
///
/// Panics if fewer than two runners successfully produce
/// output (no comparison possible) or if any two
/// successful runners produce diverging pixels.
pub fn assert_shader_agrees(
    spirv: &[u8],
    inputs: &ShaderInputs,
    tolerance: ColorTolerance,
    runners: &[&dyn ShaderRunner],
) {
    if runners.is_empty() {
        panic!("assert_shader_agrees called with no runners");
    }

    let mut results: Vec<(&'static str, ShaderOutputs)> = Vec::new();
    let mut skipped: Vec<(&'static str, String)> = Vec::new();
    let mut hard_errors: Vec<(&'static str, BackendError)> = Vec::new();

    for runner in runners {
        match runner.run(spirv, inputs) {
            Ok(out) => results.push((runner.name(), out)),
            Err(BackendError::Unsupported(why)) => {
                skipped.push((runner.name(), why));
            }
            Err(other) => {
                hard_errors.push((runner.name(), other));
            }
        }
    }

    if !hard_errors.is_empty() {
        let mut msg = String::from("backend(s) reported hard errors:\n");
        for (name, err) in &hard_errors {
            msg.push_str(&format!("  - {name}: {err}\n"));
        }
        panic!("{msg}");
    }

    if results.len() < 2 {
        let mut msg = format!(
            "assert_shader_agrees needs ≥2 successful runners; got {}\n",
            results.len(),
        );
        if !skipped.is_empty() {
            msg.push_str("  Skipped:\n");
            for (name, why) in &skipped {
                msg.push_str(&format!("    - {name}: {why}\n"));
            }
        }
        panic!("{msg}");
    }

    // Cross-compare every pair. With three runners this is
    // C(3,2)=3 comparisons, all of which we report on
    // failure for full disagreement-pattern visibility.
    let baseline_name = results[0].0;
    let baseline_pixels = &results[0].1.pixels;
    let mut failures: Vec<(String, PixelMismatch)> = Vec::new();

    for (name, out) in &results[1..] {
        if let Err(mm) = compare_buffers(baseline_pixels, &out.pixels, tolerance) {
            failures.push((format!("{baseline_name} vs {name}"), mm));
        }
    }

    if !failures.is_empty() {
        let mut msg = String::from("shader output diverges:\n");
        for (label, err) in &failures {
            msg.push_str(&format!("  - {label}: {err}\n"));
        }
        if !skipped.is_empty() {
            msg.push_str("  Skipped:\n");
            for (name, why) in &skipped {
                msg.push_str(&format!("    - {name}: {why}\n"));
            }
        }
        panic!("{msg}");
    }
}
