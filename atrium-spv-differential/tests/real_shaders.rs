//! The repo's real compute shaders (fresco-compute, the RDNA4 oracle set)
//! through both bespoke backends, compile only, on any host: each must
//! compile or return `Unsupported`. An `Internal` error is a backend bug,
//! and atrium-spv-compile treats it as a hard failure with no Cranelift
//! fallback, so the shader would not run at all.

use std::path::PathBuf;

const SHADERS: &[&str] = &[
    "fresco-compute/shaders/bin.comp.spv",
    "fresco-compute/shaders/compose.comp.spv",
    "fresco-compute/shaders/raster.comp.spv",
    "atrium-rdna4-oracle/shaders/control_flow.comp.spv",
    "atrium-rdna4-oracle/shaders/math_exact.comp.spv",
    "atrium-rdna4-oracle/shaders/raster_math.comp.spv",
    "atrium-rdna4-oracle/shaders/scalar_loads.comp.spv",
    "atrium-rdna4-oracle/shaders/subgroup_ops.comp.spv",
    "atrium-rdna4-oracle/shaders/subgroup_partial.comp.spv",
];

#[test]
fn real_shaders_never_hit_internal_errors() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut report = Vec::new();
    let mut bugs = Vec::new();
    for rel in SHADERS {
        let spirv = std::fs::read(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let module = atrium_spv_frontend::translate(&spirv).unwrap_or_else(|e| panic!("{rel}: {e:?}"));
        let arm = match atrium_spv_backend_bespoke::compile_blob(
            &module,
            atrium_spv_backend_bespoke::Target::Aarch64FreeBSD,
        ) {
            Ok(_) => "ok".to_string(),
            Err(atrium_spv_backend_bespoke::BackendError::Unsupported(m)) => format!("unsupported ({m})"),
            Err(e) => {
                bugs.push(format!("{rel} arm64: {e}"));
                "INTERNAL".to_string()
            }
        };
        let x86 = match atrium_spv_backend_bespoke_x86::compile_blob(
            &module,
            atrium_spv_backend_bespoke_x86::Target::X86_64FreeBSD,
        ) {
            Ok(_) => "ok".to_string(),
            Err(atrium_spv_backend_bespoke_x86::BackendError::Unsupported(m)) => format!("unsupported ({m})"),
            Err(e) => {
                bugs.push(format!("{rel} x86_64: {e}"));
                "INTERNAL".to_string()
            }
        };
        report.push(format!("{rel}: arm64 {arm}; x86_64 {x86}"));
    }
    eprintln!("{}", report.join("\n"));
    assert!(bugs.is_empty(), "internal errors:\n{}", bugs.join("\n"));
}
