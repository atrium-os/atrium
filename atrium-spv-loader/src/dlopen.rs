//! The dlopen + symbol-lookup half of the loader.
//!
//! Isolated in its own module so we can locally allow
//! `unsafe_code` for libloading calls without affecting
//! the rest of the crate.

#![allow(unsafe_code)]

use std::path::Path;

use atrium_spv_pcmap::PcMap;

use crate::LoadError;

/// Loaded shader handle.
///
/// Holds the dlopen `Library` (drop = unload) plus the
/// raw function pointers the daemon's rasterizer will
/// call per draw. Optionally carries the parsed PC-map
/// sidecar for crash triage.
///
/// `Send + Sync`: the function pointers themselves are
/// just bytes; calling them is the daemon's responsibility
/// and uses its own synchronization.
#[derive(Debug)]
pub struct LoadedShader {
    // Drop order matters: the function pointers must not
    // outlive the code they point into. Rust drops fields
    // in declaration order, so `entry_points` (plain
    // pointers, no-op drop) first, then `backing` last —
    // the `dlclose` / `munmap` happens after nothing can
    // call through the shader anymore.
    /// Resolved entry-point function pointers.
    pub entry_points: ShaderEntryPoints,
    /// Parsed PC-map sidecar (if the `.pcmap` file
    /// existed). `None` for shaders compiled before the
    /// pcmap-emission path landed; crash handlers
    /// downgrade to "no source attribution available."
    pub pcmap: Option<PcMap>,
    /// Whatever owns the executable code — a `dlopen`
    /// handle (the legacy `.so` / Cranelift-fallback path)
    /// or an `mmap`ed flat blob (the JIT-emit path). Held
    /// purely for its `Drop`: dropping unloads / unmaps
    /// the code. Must be the last field.
    #[allow(dead_code)]
    backing: CodeBacking,
}

/// What owns the executable code behind a [`LoadedShader`].
///
/// Two artifact paths converge here: a `.so` loaded with
/// `dlopen` (Cranelift-compiled shaders, until phase 4) or
/// a flat `atrium-spv-blob` `mmap`ed `PROT_EXEC` (the
/// bespoke JIT-emit path). Either way, dropping it
/// releases the mapping.
// The payloads are held purely for their `Drop` — nothing
// ever reads them back out — so `dead_code` is expected.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum CodeBacking {
    /// A `dlopen`ed shared library; drop = `dlclose`.
    Dlopen(libloading::Library),
    /// An `mmap`ed flat code blob; drop = `munmap`.
    Jit(crate::jitmap::JitMapping),
}

// SAFETY: both backings are Send + Sync (libloading's
// Library is; JitMapping is an immutable executable
// mapping behind a raw pointer). The function pointers in
// ShaderEntryPoints are just bytes; calling them is the
// daemon's responsibility (it locks per draw if needed).
unsafe impl Send for LoadedShader {}
unsafe impl Sync for LoadedShader {}

impl LoadedShader {
    /// Assemble a `LoadedShader` from already-resolved
    /// parts. Used by both load paths ([`open`] here and
    /// [`crate::jitmap::open`]).
    pub(crate) fn new(
        entry_points: ShaderEntryPoints,
        pcmap: Option<PcMap>,
        backing: CodeBacking,
    ) -> Self {
        Self { entry_points, pcmap, backing }
    }
}

/// Stage-specific entry-point function pointers.
///
/// Only the entry-point matching the shader's declared
/// stage is `Some`; the others are `None`.
#[derive(Debug, Clone, Copy)]
pub struct ShaderEntryPoints {
    /// `atrium_vs_main` for vertex shaders.
    pub vs_main: Option<VsMain>,
    /// `atrium_fs_main` for fragment shaders.
    pub fs_main: Option<FsMain>,
    /// `atrium_fs_main_span` — optional span (batched) fragment
    /// entry (P2).  `Some` only when the backend emitted it; the
    /// rasterizer falls back to `fs_main` when `None`.
    pub fs_span_main: Option<FsSpanMain>,
    /// `atrium_cs_main` for compute shaders.
    pub cs_main: Option<CsMain>,
}

/// Vertex-shader entry. Signature per
/// `docs/spec/tier2-renderer.md` §4.1.
pub type VsMain = unsafe extern "C" fn(
    in_attributes:     *const u8,
    in_attr_strides:   *const u32,
    uniforms:          *const u8,
    push_constants:    *const u8,
    vertex_index:      u32,
    instance_index:    u32,
    out_position:      *mut [f32; 4],
    out_varyings:      *mut u8,
    out_clip_distance: *mut f32,
);

/// Fragment-shader entry. Signature per spec §4.1.
pub type FsMain = unsafe extern "C" fn(
    in_varyings:    *const u8,
    uniforms:       *const u8,
    push_constants: *const u8,
    frag_coord_x:   f32,
    frag_coord_y:   f32,
    frag_coord_z:   f32,
    frag_coord_w:   f32,
    samples_mask:   u32,
    out_color:      *mut f32,
    out_depth:      *mut f32,
    // gl_FrontFacing: 1 = front-facing, 0 = back.  Appended
    // after the original 10 params so existing param indices
    // (out_color = 8, out_depth = 9) are unchanged.
    front_facing:   u32,
    // gl_PrimitiveID: 0-based index of the primitive within the
    // draw.  Trailing param (index 11).
    primitive_id:   u32,
);

/// Span fragment-shader entry (P2 — batched fragment execution).
///
/// Shades a run of up to `lane_count` pixels in ONE call, removing
/// the per-pixel indirect-call + per-pixel marshalling overhead.
/// Inputs/outputs are structure-of-arrays indexed by lane; a
/// `coverage_mask` bit selects which lanes are shaded (others are
/// left untouched in the output arrays).  Semantically each shaded
/// lane is identical to one `FsMain` call with the same inputs —
/// the span entry is a pure call-overhead optimization, never a
/// behaviour change.  Emitted opportunistically by backends that
/// support it (cranelift); `None` falls back to per-pixel `FsMain`.
pub type FsSpanMain = unsafe extern "C" fn(
    in_varyings_soa: *const u8,   // lane i varyings at +i*varying_stride
    varying_stride:  u32,
    uniforms:        *const u8,
    push_constants:  *const u8,
    frag_coord_x:    *const f32,  // [lane_count]
    frag_coord_y:    *const f32,
    frag_coord_z:    *const f32,
    frag_coord_w:    *const f32,
    coverage_mask:   u64,         // bit i = lane i shaded
    samples_mask:    u32,         // shared across the span
    out_color_soa:   *mut f32,    // lane i colour at +i*4 f32
    out_depth:       *mut f32,    // [lane_count]
    front_facing:    u32,         // shared per triangle
    primitive_id:    u32,         // shared per triangle
    lane_count:      u32,
);

/// Compute-shader entry. Signature per spec §4.1.
///
/// `out_buffer` is a single per-dispatch storage region the
/// shader can write through. Compute SPIR-V binds it via the
/// SSBO storage class (`StorageBuffer`); backends map any
/// access to such a variable onto this pointer. Reading from
/// it is permitted; writing is the typical use case. The
/// dispatcher (`Tier2Backend::dispatch_compute`) zeroes the
/// region before the first invocation and exposes its bytes
/// after the dispatch via `Tier2Backend::compute_output_bytes`.
pub type CsMain = unsafe extern "C" fn(
    uniforms:        *const u8,
    push_constants:  *const u8,
    out_buffer:      *mut u8,
    workgroup_id_x:  u32,
    workgroup_id_y:  u32,
    workgroup_id_z:  u32,
    local_id_x:      u32,
    local_id_y:      u32,
    local_id_z:      u32,
    // 10th arg (AAPCS64: passed on stack at SP+8): pointer to
    // a per-workgroup scratch buffer used for OpVariable in
    // StorageClass::Workgroup.  May be null when the shader
    // declares no workgroup-shared memory.
    workgroup_buf:   *mut u8,
);

/// Open a compiled shader `.so` (or `.dylib`) and resolve
/// the entry-point symbols.
///
/// `pcmap_bytes` is the contents of the sidecar `.pcmap`
/// file, if it exists. `None` is fine (crash triage just
/// downgrades).
pub(crate) fn open(
    so_path: &Path,
    pcmap_bytes: Option<&[u8]>,
) -> Result<LoadedShader, LoadError> {
    // SAFETY: dlopen is intrinsically unsafe — we trust
    // that atrium-spv-compile produced a well-formed
    // shared library + that nothing has tampered with the
    // cache between the compile and this open. The
    // production deployment runs atrium-spv-compile in a
    // jail and the cache dir is daemon-private; the trust
    // chain is enforced outside this function.
    let library = unsafe { libloading::Library::new(so_path) }
        .map_err(|e| LoadError::Internal(format!(
            "dlopen({}): {e}", so_path.display(),
        )))?;

    // Try to resolve each entry-point symbol; missing is
    // not an error (different stages export different
    // entry points).
    let vs_main: Option<VsMain> = unsafe {
        library.get::<VsMain>(b"atrium_vs_main").ok().map(|s| *s)
    };
    let fs_main: Option<FsMain> = unsafe {
        library.get::<FsMain>(b"atrium_fs_main").ok().map(|s| *s)
    };
    let fs_span_main: Option<FsSpanMain> = unsafe {
        library.get::<FsSpanMain>(b"atrium_fs_main_span").ok().map(|s| *s)
    };
    let cs_main: Option<CsMain> = unsafe {
        library.get::<CsMain>(b"atrium_cs_main").ok().map(|s| *s)
    };

    // Reject shaders that don't export any of the three.
    if vs_main.is_none() && fs_main.is_none() && cs_main.is_none() {
        return Err(LoadError::Internal(format!(
            "no atrium_(vs|fs|cs)_main symbol exported by {}",
            so_path.display(),
        )));
    }

    let pcmap = pcmap_bytes
        .map(|b| PcMap::from_bytes(b).map_err(|e| LoadError::Internal(
            format!("parsing pcmap for {}: {e}", so_path.display()),
        )))
        .transpose()?;

    Ok(LoadedShader::new(
        ShaderEntryPoints { vs_main, fs_main, fs_span_main, cs_main },
        pcmap,
        CodeBacking::Dlopen(library),
    ))
}
