//! Three-way differential harness wrappers.
//!
//! Provides `CraneliftRunner` and `BespokeRunner`
//! implementing [`atrium_spv_tests::ShaderRunner`]. Both
//! share the same lifecycle:
//!
//! 1. Frontend-translate SPIR-V → IR.
//! 2. Backend-compile IR → object bytes.
//! 3. `cc -dynamiclib`/`-shared` to produce a .dylib/.so.
//! 4. dlopen + grab `atrium_fs_main`.
//! 5. Invoke once per `inputs.varyings_per_invocation`
//!    entry (or once with no inputs), collecting the
//!    written RGBA pixels.
//!
//! Together with `InterpreterRunner` (the F1-clean oracle
//! that walks SPIR-V directly), this gives the harness
//! the three-way agreement the design called for:
//! interpreter ↔ Cranelift ↔ bespoke, no two of which
//! share frontend code.

use std::path::Path;
use std::process::Command;

use atrium_spv_tests::harness::{BackendError, ShaderRunner};
use atrium_spv_tests::interpreter::{ShaderInputs, ShaderOutputs};
use atrium_spv_tests::pixels::RgbaF32;

/// Cranelift-backed runner.
#[derive(Debug, Default)]
pub struct CraneliftRunner;

impl ShaderRunner for CraneliftRunner {
    fn name(&self) -> &'static str { "cranelift" }

    fn run(
        &self,
        spirv: &[u8],
        inputs: &ShaderInputs,
    ) -> Result<ShaderOutputs, BackendError> {
        use atrium_spv_backend_cranelift::{compile, Target};
        let module = atrium_spv_frontend::translate(spirv)
            .map_err(|e| map_unsupported_or_compile(format!("{e:?}")))?;
        let out = compile(&module, Target::host())
            .map_err(|e| map_unsupported_or_compile(format!("{e:?}")))?;
        run_via_dlopen(&out.object, inputs)
    }
}

/// Bespoke-backed runner.
#[derive(Debug, Default)]
pub struct BespokeRunner;

impl ShaderRunner for BespokeRunner {
    fn name(&self) -> &'static str { "bespoke" }

    fn run(
        &self,
        spirv: &[u8],
        inputs: &ShaderInputs,
    ) -> Result<ShaderOutputs, BackendError> {
        use atrium_spv_backend_bespoke::{compile, Target};
        let module = atrium_spv_frontend::translate(spirv)
            .map_err(|e| map_unsupported_or_compile(format!("{e:?}")))?;
        let out = compile(&module, Target::host())
            .map_err(|e| match e {
                atrium_spv_backend_bespoke::BackendError::Unsupported(s) =>
                    BackendError::Unsupported(s),
                atrium_spv_backend_bespoke::BackendError::Internal(s) =>
                    BackendError::CompileFailed(s),
            })?;
        run_via_dlopen(&out.object, inputs)
    }
}

/// Heuristic frontend-error mapping: any `Unsupported(_)`
/// in the error string downgrades to harness-Unsupported
/// (skip); anything else is a CompileFailed (hard error).
fn map_unsupported_or_compile(s: String) -> BackendError {
    if s.contains("Unsupported") {
        BackendError::Unsupported(s)
    } else {
        BackendError::CompileFailed(s)
    }
}

/// Link `object_bytes` with `cc -dynamiclib`/`-shared`,
/// dlopen, look up `atrium_fs_main`, and invoke it once
/// per invocation pulling the RGBA out_color.
fn run_via_dlopen(
    object_bytes: &[u8],
    inputs: &ShaderInputs,
) -> Result<ShaderOutputs, BackendError> {
    let dir = tempfile::tempdir()
        .map_err(|e| BackendError::Runtime(format!("tempdir: {e}")))?;
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, object_bytes)
        .map_err(|e| BackendError::Runtime(format!("write object: {e}")))?;
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path)
        .map_err(|e| BackendError::Runtime(format!("link: {e}")))?;

    let lib = unsafe { libloading::Library::new(&lib_path) }
        .map_err(|e| BackendError::Runtime(format!("dlopen: {e}")))?;
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main")
            .map_err(|e| BackendError::Runtime(
                format!("dlsym atrium_fs_main: {e}")))?
    };

    let n = inputs.varyings_per_invocation.len().max(1);
    let mut pixels: Vec<RgbaF32> = Vec::with_capacity(n);
    for _ in 0..n {
        let mut out_color = [0.0f32; 4];
        let mut out_depth = 0.0f32;
        let pc_ptr = if inputs.push_constants.iter().any(|b| *b != 0) {
            inputs.push_constants.as_ptr()
        } else {
            // Empty push-consts are passed as null for
            // ABI symmetry with shaders that don't use
            // them; non-zero contents force a real ptr.
            inputs.push_constants.as_ptr()
        };
        let uni_ptr = if inputs.uniforms.is_empty() {
            std::ptr::null()
        } else {
            inputs.uniforms.as_ptr()
        };
        unsafe {
            fs_main(
                std::ptr::null(), uni_ptr, pc_ptr,
                0.0, 0.0, 0.0, 0.0, 0,
                out_color.as_mut_ptr(), &mut out_depth,
            );
        }
        pixels.push(out_color);
    }
    Ok(ShaderOutputs { pixels })
}

fn link_to_shared_library(
    obj_path: &Path,
    lib_path: &Path,
) -> std::io::Result<()> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = Command::new("cc")
        .arg(flag)
        .arg("-o").arg(lib_path)
        .arg(obj_path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "cc {flag} failed: {status}")));
    }
    Ok(())
}
