//! Runtime helpers called from compiled tier-2 shaders.
//!
//! ## What this crate is
//!
//! The Atrium-Tier-2 software Vulkan renderer compiles
//! each shader to a position-independent native blob (see
//! `atrium-spv-backend-bespoke` + `atrium-spv-blob`).
//! Some IR ops — image sampling, image fetch — are too
//! heavy to inline in every fragment-shader function and
//! lower instead to *calls into this crate*, per the IR
//! `Op::ImageSample*` doc-comment in
//! `atrium-spv-ir`.
//!
//! ## ABI contract
//!
//! Descriptor structs are `#[repr(C)]` and the entry
//! points are `extern "C"`. A backend emits the standard
//! AAPCS64 call sequence (`bl <helper>`) and the loader
//! / JIT-emit blob path patches the helper's address into
//! a function-pointer slot in the blob's header. No deps:
//! the helpers are pure compute over raw byte buffers.
//!
//! ## What this crate is *not*
//!
//! Not a fast-path SIMD sampler. The point of this first
//! cut is correctness + a clean C-ABI for backends to
//! emit against. An inline-NEON bilinear sampler — the
//! "real" perf bar against `clang -O2` on a software
//! sampler — is a separate, later arc (the RUNBOOK
//! "texture/sampler" scoping marks it as a future
//! follow-on once this is wired through to the JIT-emit
//! path).

#![allow(clippy::missing_safety_doc)]

/// Texel formats the helpers understand. Stable wire-form
/// values (don't renumber) — a backend bakes these
/// constants into the loaded blob's descriptor table.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexFormat {
    /// 4×u8 unorm, channel order R,G,B,A.
    Rgba8Unorm = 0,
    /// 4×u8 unorm, channel order B,G,R,A. Matches the
    /// Atrium scanout buffer's hardcoded BGRA layout.
    Bgra8Unorm = 1,
    /// 1×u8 unorm, replicated to R; G=B=0, A=1.
    R8Unorm    = 2,
}

/// Storage-image texel formats the read/write helpers
/// understand.  Distinct from [`TexFormat`] (sampling): the
/// compute storage-image path needs float formats for
/// general-purpose work.  Stable wire-form values.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageFormat {
    /// 4×u8 unorm, channel order R,G,B,A.  4 bytes/texel.
    Rgba8Unorm   = 0,
    /// 1×f32, replicated to R on read; G=B=0, A=1.
    /// 4 bytes/texel.
    R32Float     = 1,
    /// 4×f32, channel order R,G,B,A.  16 bytes/texel.
    Rgba32Float  = 2,
}

impl StorageFormat {
    /// Bytes occupied by one texel in this format.
    pub fn bytes_per_texel(self) -> u32 {
        match self {
            StorageFormat::Rgba8Unorm  => 4,
            StorageFormat::R32Float    => 4,
            StorageFormat::Rgba32Float => 16,
        }
    }
}

#[inline]
fn storage_format_from_u32(v: u32) -> StorageFormat {
    match v {
        1 => StorageFormat::R32Float,
        2 => StorageFormat::Rgba32Float,
        // 0 and anything malformed -> Rgba8Unorm.
        _ => StorageFormat::Rgba8Unorm,
    }
}

/// A storage-image binding (`image2D` / `image3D`).  Unlike
/// [`TexDesc`], `data` is `*mut` — `OpImageWrite` mutates it.
/// Texels are addressed by pure integer arithmetic
/// (`data + z*slice_bytes + y*stride_bytes + x*bytes_per_texel`);
/// there is no sampler and no filtering.
///
/// The first five fields (`data`..`format`, bytes 0..24) are
/// the original v1 2D layout — backends that only read those
/// offsets stay binary-compatible.  `depth` / `slice_bytes`
/// were appended for `image3D`: a 2D image sets `depth = 1`
/// and `slice_bytes = 0`.  `mip_count` / `mip_descs` were
/// appended for mip-level storage images (Arc 26).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ImageDesc {
    pub data:         *mut u8,
    pub width:        u32,
    pub height:       u32,
    pub stride_bytes: u32,
    /// `StorageFormat` as `u32` for C-ABI portability.
    pub format:       u32,
    /// Number of Z slices (1 for a 2D image).
    pub depth:        u32,
    /// Byte stride between consecutive Z slices.  Ignored
    /// when `depth == 1`; conventionally `height * stride_bytes`.
    pub slice_bytes:  u32,
    /// Number of mip levels.  `0` or `1` means single-level
    /// (the base `data`/`width`/`height`/etc. describe the
    /// only mip); the `*_lod` helpers ignore `mip_descs` in
    /// that case and operate on this `ImageDesc` directly.
    pub mip_count:    u32,
    /// Pointer to an array of per-mip `ImageDesc`s, indexed
    /// `0..mip_count`.  Slot `i` holds the descriptor for
    /// mip level `i`: distinct `data` pointer + dimensions
    /// per level.  May be null when `mip_count <= 1`.
    pub mip_descs:    *const ImageDesc,
}

/// Sampler filter modes. Wire-form values are stable.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    Nearest = 0,
    Linear  = 1,
}

/// Texture-coordinate wrap modes at the [0,1] border.
/// Wire-form values are stable.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    /// Clamp to [0, width-1] / [0, height-1].
    ClampToEdge = 0,
    /// Modular wrap.
    Repeat      = 1,
    /// Triangle wave wrap (mirror at each boundary).
    Mirror      = 2,
}

/// A 2D image binding. `data` points at a row-major texel
/// buffer of `height` rows, each `stride_bytes` long; the
/// pixel format determines bytes-per-texel within a row.
/// The shader sees this as a `texture2D` / `image2D`.
///
/// `mip_count` / `mip_descs` were appended for multi-mip
/// sampling (Arc 29).  Single-mip callers set them to 0 /
/// null; the `*_lod` helpers fall back to the base
/// descriptor when `lod` is out of range.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TexDesc {
    pub data:         *const u8,
    pub width:        u32,
    pub height:       u32,
    pub stride_bytes: u32,
    /// `TexFormat` as `u32` for C-ABI portability.
    pub format:       u32,
    /// Number of mip levels (0 or 1 means single-level).
    pub mip_count:    u32,
    /// Pointer to an array of per-mip `TexDesc`s indexed
    /// `0..mip_count`; mirrors `ImageDesc.mip_descs`.  May
    /// be null when `mip_count <= 1`.
    pub mip_descs:    *const TexDesc,
    /// Number of array layers (1 for a non-array texture).
    /// Appended in Arc 30 for `sampler2DArray` support.
    pub depth:        u32,
    /// Byte stride between consecutive array layers.
    /// Conventionally `height * stride_bytes`; ignored when
    /// `depth == 1`.
    pub slice_bytes:  u32,
}

/// A sampler binding. Independent of any specific image,
/// per the Vulkan combined-image-sampler model.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SamplerDesc {
    /// `FilterMode` as `u32`.
    pub mag_filter: u32,
    /// `FilterMode` as `u32`.
    pub min_filter: u32,
    /// `WrapMode` as `u32`.
    pub wrap_s:     u32,
    /// `WrapMode` as `u32`.
    pub wrap_t:     u32,
}

// ── Uniforms-buffer layout (v1) ───────────────────────────
//
// How a compiled shader finds its bound textures + samplers
// + the runtime helpers to call. The Atrium-Tier-2
// fragment-shader AAPCS64 split passes a `uniforms` pointer
// in X1 (per docs/spec/tier2-renderer.md §4.1). v1 of the
// texture path overlays the buffer's prefix with:
//
//   bytes  0..40 : runtime-helper function pointers
//                  ( 0:  atrium_tex_sample_2d
//                    8:  atrium_tex_fetch_2d
//                   16:  atrium_tex_sample_2d_lod
//                   24:  atrium_tex_sample_2d_array
//                   32:  atrium_tex_sample_cube )
//   bytes 40..   : flat descriptor table — slot `B` at
//                  byte `UNIFORMS_DESC_BASE + B*16`,
//                  carrying ( 0: tex_desc*, 8: samp_desc* )
//
// A backend that sees an `ImageSample` against binding `B`
// emits the call-via-pointer sequence:
//
//     ldr  x_fn,   [X1]                    ; helper ptr
//     ldr  x_tex,  [X1, #16 + B*16]        ; tex_desc*
//     ldr  x_samp, [X1, #16 + B*16 + 8]    ; samp_desc*
//     blr  x_fn
//
// The deliberate `blr <reg>` (not `bl <symbol>`) keeps the
// emitted code reloc-free, so the bespoke JIT-emit blob
// path works unchanged — the host caller patches in the
// helper addresses at descriptor-table build time. The
// Cranelift `compile()` (object → cc → dlopen) path uses
// the same mechanism rather than relying on `cc`'s
// dynamic-linker to resolve the runtime crate; this keeps
// the two backends on a single descriptor-table ABI.
//
// Shaders that *also* read a uniform block must arrange for
// it to live past the descriptor table (set the buffer's
// binding offsets accordingly). Multi-set support + a
// dedicated descriptor-table register (X6) land later when
// a real shader needs both.

/// Offset of the runtime-helper pointer block at the start
/// of the uniforms buffer. `uniforms + 0` is
/// `atrium_tex_sample_2d`; `uniforms + 8` is
/// `atrium_tex_fetch_2d`.
pub const UNIFORMS_HELPERS_BASE: usize = 0;

/// Offset of the descriptor table within the uniforms
/// buffer. Slot `B` lives at `UNIFORMS_DESC_BASE + B * DESC_SLOT_BYTES`.
/// Grew from 16 → 24 (Arc 29 sample_2d_lod) → 32 (Arc 30
/// sample_2d_array) → 40 (Arc 31 sample_cube).
pub const UNIFORMS_DESC_BASE: usize = 40;

/// Bytes per descriptor slot in the table. Two 64-bit
/// pointers packed back-to-back — `tex_desc*` then
/// `samp_desc*`.
pub const DESC_SLOT_BYTES: usize = 16;

/// Build a uniforms-buffer prefix sized for `count`
/// descriptor slots, including the 16-byte runtime-helper
/// header. Caller fills helper pointers with
/// [`write_helper_pointers`] and descriptor slots with
/// [`write_descriptor_slot`].
pub fn descriptor_table_buffer(count: usize) -> Vec<u8> {
    vec![0u8; UNIFORMS_DESC_BASE + count * DESC_SLOT_BYTES]
}

// ── Compute storage-image descriptor table ────────────────
//
// Compute shaders that use `image2D` / `image3D` storage
// images get a SEPARATE descriptor table, passed in the X0
// slot (which the compute calling convention otherwise leaves
// null).  Layout:
//
//   bytes  0..64 : runtime-helper pointers
//                  ( 0:  atrium_img_read_2d
//                    8:  atrium_img_write_2d
//                   16:  atrium_img_read_3d
//                   24:  atrium_img_write_3d
//                   32:  atrium_img_read_2d_lod
//                   40:  atrium_img_write_2d_lod
//                   48:  atrium_img_read_3d_lod
//                   56:  atrium_img_write_3d_lod )
//   bytes 64..   : descriptor table — slot `B` at byte
//                  `IMG_TABLE_DESC_BASE + B*IMG_DESC_SLOT_BYTES`,
//                  holding a single `ImageDesc*`.
//
// Storage images have no sampler, so each slot is one
// pointer (8 bytes) rather than the texture table's pair.

/// Offset of the descriptor table within the compute
/// storage-image table buffer.  Grew from 32 to 64 when the
/// four `_lod` mip-aware helpers were added in Arc 26.
pub const IMG_TABLE_DESC_BASE: usize = 64;

/// Bytes per storage-image descriptor slot — one
/// `ImageDesc*`.
pub const IMG_DESC_SLOT_BYTES: usize = 8;

/// Build a compute storage-image table buffer sized for
/// `count` image bindings, including the 16-byte helper
/// header.  Fill helpers with [`write_image_helper_pointers`]
/// and slots with [`write_image_descriptor_slot`].
pub fn image_table_buffer(count: usize) -> Vec<u8> {
    vec![0u8; IMG_TABLE_DESC_BASE + count * IMG_DESC_SLOT_BYTES]
}

/// Write the eight storage-image helper pointers
/// (2D / 3D × read / write × base / lod) into a compute
/// storage-image table.  The four `_lod` helpers take an
/// extra `lod: i32` argument and indirect through
/// `ImageDesc.mip_descs[lod]` when `lod < mip_count`.
///
/// # Safety
/// The function pointers must outlive every shader
/// invocation that uses this buffer.
#[allow(clippy::too_many_arguments)]
pub unsafe fn write_image_helper_pointers(
    buf: &mut [u8],
    read_2d:  unsafe extern "C" fn(*const ImageDesc, i32, i32, *mut f32),
    write_2d: unsafe extern "C" fn(*const ImageDesc, i32, i32, *const f32),
    read_3d:  unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, *mut f32),
    write_3d: unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, *const f32),
    read_2d_lod: unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, *mut f32),
    write_2d_lod: unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, *const f32),
    read_3d_lod: unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, i32, *mut f32),
    write_3d_lod: unsafe extern "C" fn(
        *const ImageDesc, i32, i32, i32, i32, *const f32),
) {
    assert!(buf.len() >= IMG_TABLE_DESC_BASE,
        "image table buffer too small for the helper header");
    let r2  = (read_2d      as usize as u64).to_le_bytes();
    let w2  = (write_2d     as usize as u64).to_le_bytes();
    let r3  = (read_3d      as usize as u64).to_le_bytes();
    let w3  = (write_3d     as usize as u64).to_le_bytes();
    let r2l = (read_2d_lod  as usize as u64).to_le_bytes();
    let w2l = (write_2d_lod as usize as u64).to_le_bytes();
    let r3l = (read_3d_lod  as usize as u64).to_le_bytes();
    let w3l = (write_3d_lod as usize as u64).to_le_bytes();
    buf[ 0.. 8].copy_from_slice(&r2);
    buf[ 8..16].copy_from_slice(&w2);
    buf[16..24].copy_from_slice(&r3);
    buf[24..32].copy_from_slice(&w3);
    buf[32..40].copy_from_slice(&r2l);
    buf[40..48].copy_from_slice(&w2l);
    buf[48..56].copy_from_slice(&r3l);
    buf[56..64].copy_from_slice(&w3l);
}

/// Write an `ImageDesc*` at binding slot `slot`.
///
/// # Safety
/// `img` must outlive every shader invocation using this
/// buffer.
pub unsafe fn write_image_descriptor_slot(
    buf: &mut [u8],
    slot: usize,
    img: *const ImageDesc,
) {
    let base = IMG_TABLE_DESC_BASE + slot * IMG_DESC_SLOT_BYTES;
    assert!(buf.len() >= base + IMG_DESC_SLOT_BYTES,
        "image table buffer too small for slot {slot}");
    let bytes = (img as usize as u64).to_le_bytes();
    buf[base..base + 8].copy_from_slice(&bytes);
}

/// Write the runtime-helper function-pointer header.  Four
/// helpers: `atrium_tex_sample_2d` (implicit-LOD; always
/// mip 0), `atrium_tex_fetch_2d` (texel-fetch with LOD),
/// `atrium_tex_sample_2d_lod` (explicit-LOD with mip
/// selection), and `atrium_tex_sample_2d_array` (implicit-
/// LOD `sampler2DArray` with a `layer: f32` arg, mip 0).
///
/// # Safety
/// The function pointers must remain valid for the lifetime
/// of every shader invocation that uses this buffer.
#[allow(clippy::too_many_arguments)]
pub unsafe fn write_helper_pointers(
    buf: &mut [u8],
    sample_2d: unsafe extern "C" fn(
        *const TexDesc, *const SamplerDesc, f32, f32, *mut f32),
    fetch_2d: unsafe extern "C" fn(
        *const TexDesc, i32, i32, i32, *mut f32),
    sample_2d_lod: unsafe extern "C" fn(
        *const TexDesc, *const SamplerDesc, f32, f32, f32, *mut f32),
    sample_2d_array: unsafe extern "C" fn(
        *const TexDesc, *const SamplerDesc, f32, f32, f32, *mut f32),
    sample_cube: unsafe extern "C" fn(
        *const TexDesc, *const SamplerDesc, f32, f32, f32, *mut f32),
) {
    assert!(buf.len() >= UNIFORMS_DESC_BASE,
        "uniforms buffer too small for the helper header");
    let s  = (sample_2d       as usize as u64).to_le_bytes();
    let f  = (fetch_2d        as usize as u64).to_le_bytes();
    let sl = (sample_2d_lod   as usize as u64).to_le_bytes();
    let sa = (sample_2d_array as usize as u64).to_le_bytes();
    let sc = (sample_cube     as usize as u64).to_le_bytes();
    buf[ 0.. 8].copy_from_slice(&s);
    buf[ 8..16].copy_from_slice(&f);
    buf[16..24].copy_from_slice(&sl);
    buf[24..32].copy_from_slice(&sa);
    buf[32..40].copy_from_slice(&sc);
}

/// Write a `(tex_desc, samp_desc)` pointer pair at binding
/// slot `slot`. The buffer must be
/// `>= UNIFORMS_DESC_BASE + (slot + 1) * DESC_SLOT_BYTES`
/// long.
///
/// # Safety
/// The host pointers must outlive every shader invocation
/// that reads them through this table.
pub unsafe fn write_descriptor_slot(
    buf: &mut [u8],
    slot: usize,
    tex: *const TexDesc,
    samp: *const SamplerDesc,
) {
    let base = UNIFORMS_DESC_BASE + slot * DESC_SLOT_BYTES;
    assert!(buf.len() >= base + DESC_SLOT_BYTES,
        "descriptor-table buffer too small for slot {slot}");
    let tex_bytes  = (tex as usize as u64).to_le_bytes();
    let samp_bytes = (samp as usize as u64).to_le_bytes();
    buf[base    .. base +  8].copy_from_slice(&tex_bytes);
    buf[base + 8.. base + 16].copy_from_slice(&samp_bytes);
}

// ── Helpers ───────────────────────────────────────────────

/// Sample a 2D image at normalised UV coordinates with
/// implicit LOD (LOD computation deferred — this v1
/// always samples mip 0). Writes RGBA32F into
/// `out_rgba[0..4]`.
///
/// # Safety
/// * `tex` and `samp` must be valid pointers.
/// * `tex.data` must point at `>= tex.height * tex.stride_bytes`
///   readable bytes.
/// * `out_rgba` must point at `>= 16` writable bytes.
/// * `tex.format` must be a valid `TexFormat` discriminant.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_sample_2d(
    tex: *const TexDesc,
    samp: *const SamplerDesc,
    u: f32, v: f32,
    out_rgba: *mut f32,
) {
    let t = &*tex;
    let s = &*samp;
    let rgba = sample_2d_impl(t, s, u, v);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// Resolve a `(base TexDesc, lod)` pair to the descriptor
/// that holds the texel data for that mip level.  Falls
/// back to the base descriptor when `lod` is out of range
/// (or `mip_descs` is null), keeping behaviour well-defined
/// for single-mip bindings.
#[inline]
unsafe fn pick_tex_mip<'a>(tex: &'a TexDesc, lod: i32) -> &'a TexDesc {
    if lod <= 0 || tex.mip_count <= 1 || tex.mip_descs.is_null() {
        return tex;
    }
    let l = lod as u32;
    if l >= tex.mip_count {
        return tex;
    }
    &*tex.mip_descs.add(l as usize)
}

/// Fetch a single texel by integer coordinates (no
/// filtering, no wrap — the caller is responsible for
/// keeping `(x, y)` in range).  `lod` selects the mip
/// level via `TexDesc.mip_descs[lod]` when in range;
/// otherwise the base descriptor is used.
///
/// # Safety
/// As for `atrium_tex_sample_2d`. Additionally, `x` and
/// `y` must be in `[0, tex.width)` × `[0, tex.height)` of
/// the selected mip level.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_fetch_2d(
    tex: *const TexDesc,
    x: i32, y: i32, lod: i32,
    out_rgba: *mut f32,
) {
    let t = pick_tex_mip(&*tex, lod);
    let rgba = fetch_texel_impl(t, x as u32, y as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// Sample a `samplerCube` with a vec3 direction.  The
/// helper performs the standard cubemap face selection:
/// the major axis (largest `|component|`) names the face;
/// the other two components, divided by the major and
/// remapped to `[0, 1]`, become the (u, v) coords of that
/// face's 2D image.  Per-face data sits at
/// `tex.data + face * tex.slice_bytes` (i.e. `tex.depth`
/// should be 6 and `slice_bytes` should describe one face's
/// byte stride).
///
/// Face order matches GL_KHR_storage_cube_compatible /
/// Vulkan's array-of-faces convention:
///   0 = +X, 1 = -X, 2 = +Y, 3 = -Y, 4 = +Z, 5 = -Z.
///
/// # Safety
/// As for `atrium_tex_sample_2d`; additionally `tex.depth`
/// must be ≥ 6 and `tex.slice_bytes` the per-face stride.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_sample_cube(
    tex: *const TexDesc,
    samp: *const SamplerDesc,
    x: f32, y: f32, z: f32,
    out_rgba: *mut f32,
) {
    let t_base = &*tex;
    let ax = x.abs();
    let ay = y.abs();
    let az = z.abs();
    // Pick major axis -> face index + (sc, tc, ma).  Tie-
    // breaking matches the Vulkan reference order.
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if x >= 0.0 { (0u32, -z, -y, ax) } // +X
        else        { (1u32,  z, -y, ax) } // -X
    } else if ay >= az {
        if y >= 0.0 { (2u32,  x,  z, ay) } // +Y
        else        { (3u32,  x, -z, ay) } // -Y
    } else if z >= 0.0 {
        (4u32,  x, -y, az)                 // +Z
    } else {
        (5u32, -x, -y, az)                 // -Z
    };
    // Avoid divide-by-zero on a (0, 0, 0) direction.
    let inv_ma = if ma == 0.0 { 0.0 } else { 1.0 / ma };
    let u = 0.5 * (sc * inv_ma + 1.0);
    let v = 0.5 * (tc * inv_ma + 1.0);
    let slice_off = (face as usize) * (t_base.slice_bytes as usize);
    let mut face_tex = *t_base;
    face_tex.data = t_base.data.add(slice_off);
    let s = &*samp;
    let rgba = sample_2d_impl(&face_tex, s, u, v);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// Sample one layer of a `sampler2DArray` (implicit LOD,
/// always mip 0).  The layer index is `round(layer)`,
/// clamped to `[0, tex.depth - 1]`.  Per-layer data sits at
/// `tex.data + layer * tex.slice_bytes`.
///
/// # Safety
/// As for `atrium_tex_sample_2d`; additionally `tex.depth`
/// must accurately count layers and `tex.slice_bytes` the
/// per-layer byte stride.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_sample_2d_array(
    tex: *const TexDesc,
    samp: *const SamplerDesc,
    u: f32, v: f32, layer: f32,
    out_rgba: *mut f32,
) {
    let t_base = &*tex;
    let layers = t_base.depth.max(1);
    let l_idx = (layer.round() as i32)
        .clamp(0, (layers as i32) - 1) as u32;
    let slice_off = (l_idx as usize) * (t_base.slice_bytes as usize);
    let mut t = *t_base;
    t.data = t_base.data.add(slice_off);
    let s = &*samp;
    let rgba = sample_2d_impl(&t, s, u, v);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// Sample a 2D image with explicit LOD.  `lod` is rounded
/// to the nearest integer mip level and the descriptor for
/// that mip is selected via `TexDesc.mip_descs[lod]`;
/// out-of-range `lod` falls back to the base descriptor.
/// Within the picked mip, sampling proceeds exactly as
/// [`atrium_tex_sample_2d`].
///
/// # Safety
/// As for `atrium_tex_sample_2d`.
#[no_mangle]
pub unsafe extern "C" fn atrium_tex_sample_2d_lod(
    tex: *const TexDesc,
    samp: *const SamplerDesc,
    u: f32, v: f32, lod: f32,
    out_rgba: *mut f32,
) {
    // Round-to-nearest int LOD for mip selection (single-
    // level fallback when out of range).
    let lod_i = lod.round() as i32;
    let t = pick_tex_mip(&*tex, lod_i);
    let s = &*samp;
    let rgba = sample_2d_impl(t, s, u, v);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

// ── Safe Rust wrappers ────────────────────────────────────
//
// The `extern "C"` entry points above are what compiled
// shaders call (via the C ABI). Rust callers — including
// the atrium-spv-tests interpreter — get a safe entry that
// borrows the descriptors and `data` as slices, so consumers
// can stay `#![forbid(unsafe_code)]`.

/// Safe wrapper around [`atrium_tex_sample_2d`]. Borrows
/// `data`, `tex`, and `samp`; never inspects bytes past
/// `tex.height * tex.stride_bytes`.
pub fn sample_2d(
    data: &[u8],
    tex: &TexDesc,
    samp: &SamplerDesc,
    u: f32, v: f32,
) -> [f32; 4] {
    debug_assert!(data.len() >= (tex.height as usize) * (tex.stride_bytes as usize),
        "TexDesc dimensions overrun the data slice");
    let mut t = *tex;
    t.data = data.as_ptr();
    sample_2d_impl(&t, samp, u, v)
}

/// Safe wrapper around [`atrium_tex_fetch_2d`]. Caller is
/// responsible for `(x, y) ∈ [0, width) × [0, height)` —
/// out-of-range falls back to edge-clamp behaviour.
pub fn fetch_2d(
    data: &[u8],
    tex: &TexDesc,
    x: i32, y: i32, _lod: i32,
) -> [f32; 4] {
    debug_assert!(data.len() >= (tex.height as usize) * (tex.stride_bytes as usize),
        "TexDesc dimensions overrun the data slice");
    let mut t = *tex;
    t.data = data.as_ptr();
    fetch_texel_impl(&t, x as u32, y as u32)
}

// ── Storage-image read / write (compute) ──────────────────
//
// `OpImageRead` / `OpImageWrite` on an `image2D`.  No
// sampler, no filtering — the texel address is
// `data + y*stride_bytes + x*bytes_per_texel`.  Out-of-range
// coordinates are clamped to the edge (defensive: Vulkan
// leaves them undefined, but UB in a software path is worse).

/// Unpack one storage-image texel to RGBA32F.
fn image_read_impl(img: &ImageDesc, x: u32, y: u32) -> [f32; 4] {
    let fmt = storage_format_from_u32(img.format);
    let xc = x.min(img.width.saturating_sub(1));
    let yc = y.min(img.height.saturating_sub(1));
    let row_off = yc as usize * img.stride_bytes as usize;
    let bpt = fmt.bytes_per_texel() as usize;
    unsafe {
        let p = (img.data as *const u8).add(row_off + xc as usize * bpt);
        match fmt {
            StorageFormat::Rgba8Unorm => [
                u8_to_unorm(*p.add(0)), u8_to_unorm(*p.add(1)),
                u8_to_unorm(*p.add(2)), u8_to_unorm(*p.add(3)),
            ],
            StorageFormat::R32Float => {
                let r = f32::from_le_bytes([
                    *p.add(0), *p.add(1), *p.add(2), *p.add(3)]);
                [r, 0.0, 0.0, 1.0]
            }
            StorageFormat::Rgba32Float => {
                let mut out = [0.0f32; 4];
                for (k, o) in out.iter_mut().enumerate() {
                    let b = p.add(k * 4);
                    *o = f32::from_le_bytes([
                        *b.add(0), *b.add(1), *b.add(2), *b.add(3)]);
                }
                out
            }
        }
    }
}

/// Pack an RGBA32F texel into a storage image.
fn image_write_impl(img: &ImageDesc, x: u32, y: u32, rgba: [f32; 4]) {
    let fmt = storage_format_from_u32(img.format);
    let xc = x.min(img.width.saturating_sub(1));
    let yc = y.min(img.height.saturating_sub(1));
    let row_off = yc as usize * img.stride_bytes as usize;
    let bpt = fmt.bytes_per_texel() as usize;
    unsafe {
        let p = img.data.add(row_off + xc as usize * bpt);
        match fmt {
            StorageFormat::Rgba8Unorm => {
                for k in 0..4 {
                    *p.add(k) = unorm_to_u8(rgba[k]);
                }
            }
            StorageFormat::R32Float => {
                let b = rgba[0].to_le_bytes();
                for k in 0..4 { *p.add(k) = b[k]; }
            }
            StorageFormat::Rgba32Float => {
                for c in 0..4 {
                    let b = rgba[c].to_le_bytes();
                    for k in 0..4 { *p.add(c * 4 + k) = b[k]; }
                }
            }
        }
    }
}

/// `OpImageRead`: read a texel from a storage image at
/// integer coords.  Writes RGBA32F into `out_rgba[0..4]`.
///
/// # Safety
/// * `img` must be valid; `img.data` must point at
///   `>= img.height * img.stride_bytes` readable bytes.
/// * `out_rgba` must point at `>= 16` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_read_2d(
    img: *const ImageDesc,
    x: i32, y: i32,
    out_rgba: *mut f32,
) {
    let rgba = image_read_impl(&*img, x as u32, y as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// `OpImageWrite`: write a texel to a storage image at
/// integer coords.  `in_rgba[0..4]` is the RGBA32F value;
/// narrower formats drop the unused lanes.
///
/// # Safety
/// * `img` must be valid; `img.data` must point at
///   `>= img.height * img.stride_bytes` writable bytes.
/// * `in_rgba` must point at `>= 16` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_write_2d(
    img: *const ImageDesc,
    x: i32, y: i32,
    in_rgba: *const f32,
) {
    let src = std::slice::from_raw_parts(in_rgba, 4);
    let rgba = [src[0], src[1], src[2], src[3]];
    image_write_impl(&*img, x as u32, y as u32, rgba);
}

/// Unpack one image3D texel to RGBA32F.
fn image_read_impl_3d(
    img: &ImageDesc, x: u32, y: u32, z: u32,
) -> [f32; 4] {
    let zc = z.min(img.depth.saturating_sub(1));
    let slice_off = zc as usize * img.slice_bytes as usize;
    // Reuse the 2D impl by temporarily rebasing data to the
    // start of the chosen Z slice.
    let mut i = *img;
    unsafe { i.data = img.data.add(slice_off); }
    image_read_impl(&i, x, y)
}

/// Pack an RGBA32F texel into an image3D.
fn image_write_impl_3d(
    img: &ImageDesc, x: u32, y: u32, z: u32, rgba: [f32; 4],
) {
    let zc = z.min(img.depth.saturating_sub(1));
    let slice_off = zc as usize * img.slice_bytes as usize;
    let mut i = *img;
    unsafe { i.data = img.data.add(slice_off); }
    image_write_impl(&i, x, y, rgba);
}

/// `OpImageRead` on an `image3D`: read a texel from a 3D
/// storage image at integer (x, y, z).  Writes RGBA32F into
/// `out_rgba[0..4]`.
///
/// # Safety
/// As for [`atrium_img_read_2d`]; additionally
/// `img.slice_bytes` must be set and `img.data` must point
/// at `>= img.depth * img.slice_bytes` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_read_3d(
    img: *const ImageDesc,
    x: i32, y: i32, z: i32,
    out_rgba: *mut f32,
) {
    let rgba = image_read_impl_3d(
        &*img, x as u32, y as u32, z as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// `OpImageWrite` on an `image3D`: write a texel at integer
/// (x, y, z).
///
/// # Safety
/// As for [`atrium_img_write_2d`]; additionally
/// `img.slice_bytes` must be set and `img.data` must point
/// at `>= img.depth * img.slice_bytes` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_write_3d(
    img: *const ImageDesc,
    x: i32, y: i32, z: i32,
    in_rgba: *const f32,
) {
    let src = std::slice::from_raw_parts(in_rgba, 4);
    let rgba = [src[0], src[1], src[2], src[3]];
    image_write_impl_3d(
        &*img, x as u32, y as u32, z as u32, rgba);
}

/// Resolve a `(base ImageDesc, lod)` pair to the descriptor
/// that actually holds the texel data for that mip level.
/// Falls back to the base descriptor when `lod` is out of
/// range (or `mip_descs` is null), keeping behaviour
/// well-defined for ill-formed bindings.
#[inline]
unsafe fn pick_mip<'a>(img: &'a ImageDesc, lod: i32) -> &'a ImageDesc {
    if lod <= 0 || img.mip_count <= 1 || img.mip_descs.is_null() {
        return img;
    }
    let l = lod as u32;
    if l >= img.mip_count {
        return img;
    }
    &*img.mip_descs.add(l as usize)
}

/// `OpImageRead` with `Image-Operands::Lod` on an `image2D`.
///
/// # Safety
/// As for [`atrium_img_read_2d`]; additionally, when `lod > 0`,
/// `img.mip_descs[lod]` must be a valid `ImageDesc` for the
/// chosen mip level.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_read_2d_lod(
    img: *const ImageDesc,
    x: i32, y: i32, lod: i32,
    out_rgba: *mut f32,
) {
    let m = pick_mip(&*img, lod);
    let rgba = image_read_impl(m, x as u32, y as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// `OpImageWrite` with `Image-Operands::Lod` on an `image2D`.
///
/// # Safety
/// As for [`atrium_img_write_2d`]; additionally, when `lod > 0`,
/// the descriptor at `img.mip_descs[lod]` is mutated.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_write_2d_lod(
    img: *const ImageDesc,
    x: i32, y: i32, lod: i32,
    in_rgba: *const f32,
) {
    let m = pick_mip(&*img, lod);
    let src = std::slice::from_raw_parts(in_rgba, 4);
    let rgba = [src[0], src[1], src[2], src[3]];
    image_write_impl(m, x as u32, y as u32, rgba);
}

/// `OpImageRead` with `Image-Operands::Lod` on an `image3D`.
///
/// # Safety
/// As for [`atrium_img_read_3d`]; mip-level selection mirrors
/// the 2D Lod helper.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_read_3d_lod(
    img: *const ImageDesc,
    x: i32, y: i32, z: i32, lod: i32,
    out_rgba: *mut f32,
) {
    let m = pick_mip(&*img, lod);
    let rgba = image_read_impl_3d(m, x as u32, y as u32, z as u32);
    let out = std::slice::from_raw_parts_mut(out_rgba, 4);
    out.copy_from_slice(&rgba);
}

/// `OpImageWrite` with `Image-Operands::Lod` on an `image3D`.
///
/// # Safety
/// As for [`atrium_img_write_3d`]; mip-level selection mirrors
/// the 2D Lod helper.
#[no_mangle]
pub unsafe extern "C" fn atrium_img_write_3d_lod(
    img: *const ImageDesc,
    x: i32, y: i32, z: i32, lod: i32,
    in_rgba: *const f32,
) {
    let m = pick_mip(&*img, lod);
    let src = std::slice::from_raw_parts(in_rgba, 4);
    let rgba = [src[0], src[1], src[2], src[3]];
    image_write_impl_3d(m, x as u32, y as u32, z as u32, rgba);
}

/// Safe wrapper around [`atrium_img_read_2d`].
pub fn image_read_2d(
    data: &[u8],
    img: &ImageDesc,
    x: i32, y: i32,
) -> [f32; 4] {
    debug_assert!(data.len() >= (img.height as usize) * (img.stride_bytes as usize),
        "ImageDesc dimensions overrun the data slice");
    let mut i = *img;
    i.data = data.as_ptr() as *mut u8;
    image_read_impl(&i, x as u32, y as u32)
}

/// Safe wrapper around [`atrium_img_write_2d`].
pub fn image_write_2d(
    data: &mut [u8],
    img: &ImageDesc,
    x: i32, y: i32,
    rgba: [f32; 4],
) {
    debug_assert!(data.len() >= (img.height as usize) * (img.stride_bytes as usize),
        "ImageDesc dimensions overrun the data slice");
    let mut i = *img;
    i.data = data.as_mut_ptr();
    image_write_impl(&i, x as u32, y as u32, rgba);
}

#[inline]
fn unorm_to_u8(f: f32) -> u8 {
    // Clamp to [0,1] then round-to-nearest.
    (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

// ── Implementation (safe Rust, called from the FFI wrappers) ──

fn sample_2d_impl(t: &TexDesc, s: &SamplerDesc, u: f32, v: f32) -> [f32; 4] {
    // The "implicit LOD" decision (mag vs min filter) needs
    // fragment derivatives. v1 doesn't expose them yet, so
    // we always pick mag_filter — the common case for the
    // 1:1-mapped pixel passes the renderer is built for.
    let filter = filter_from_u32(s.mag_filter);
    let wrap_s = wrap_from_u32(s.wrap_s);
    let wrap_t = wrap_from_u32(s.wrap_t);
    // Map normalised UV → texel space. Vulkan/SPIR-V
    // convention: `(0,0)` is the top-left of texel
    // `(0,0)`'s top-left corner, `(1,1)` the bottom-right
    // of the last texel — so sample centres sit at
    // `(i+0.5, j+0.5)` and the continuous coordinate is
    // `u*w - 0.5`.
    let x = u * t.width as f32 - 0.5;
    let y = v * t.height as f32 - 0.5;
    match filter {
        FilterMode::Nearest => {
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            let (xi, yi) = (
                apply_wrap(xi, t.width as i32, wrap_s),
                apply_wrap(yi, t.height as i32, wrap_t),
            );
            fetch_texel_impl(t, xi as u32, yi as u32)
        }
        FilterMode::Linear => {
            // Bilinear: 4 texel taps + bilerp.
            let x0 = x.floor() as i32;
            let y0 = y.floor() as i32;
            let fx = x - x0 as f32;
            let fy = y - y0 as f32;
            let x1 = x0 + 1;
            let y1 = y0 + 1;
            let x0w = apply_wrap(x0, t.width as i32, wrap_s) as u32;
            let x1w = apply_wrap(x1, t.width as i32, wrap_s) as u32;
            let y0w = apply_wrap(y0, t.height as i32, wrap_t) as u32;
            let y1w = apply_wrap(y1, t.height as i32, wrap_t) as u32;
            let t00 = fetch_texel_impl(t, x0w, y0w);
            let t10 = fetch_texel_impl(t, x1w, y0w);
            let t01 = fetch_texel_impl(t, x0w, y1w);
            let t11 = fetch_texel_impl(t, x1w, y1w);
            let mut out = [0.0f32; 4];
            for k in 0..4 {
                let top = t00[k] * (1.0 - fx) + t10[k] * fx;
                let bot = t01[k] * (1.0 - fx) + t11[k] * fx;
                out[k] = top * (1.0 - fy) + bot * fy;
            }
            out
        }
    }
}

fn fetch_texel_impl(t: &TexDesc, x: u32, y: u32) -> [f32; 4] {
    // Caller-clamped: `apply_wrap` already brought (x, y)
    // into [0, w) × [0, h). We treat any out-of-range
    // remnant as edge (defensive — better than UB).
    let xc = x.min(t.width.saturating_sub(1));
    let yc = y.min(t.height.saturating_sub(1));
    let row_off = yc as usize * t.stride_bytes as usize;
    let fmt = format_from_u32(t.format);
    unsafe {
        let row_ptr = t.data.add(row_off);
        match fmt {
            TexFormat::Rgba8Unorm => {
                let px_ptr = row_ptr.add(xc as usize * 4);
                [
                    u8_to_unorm(*px_ptr.add(0)),
                    u8_to_unorm(*px_ptr.add(1)),
                    u8_to_unorm(*px_ptr.add(2)),
                    u8_to_unorm(*px_ptr.add(3)),
                ]
            }
            TexFormat::Bgra8Unorm => {
                let px_ptr = row_ptr.add(xc as usize * 4);
                [
                    u8_to_unorm(*px_ptr.add(2)), // R from byte 2
                    u8_to_unorm(*px_ptr.add(1)), // G
                    u8_to_unorm(*px_ptr.add(0)), // B from byte 0
                    u8_to_unorm(*px_ptr.add(3)), // A
                ]
            }
            TexFormat::R8Unorm => {
                let r = u8_to_unorm(*row_ptr.add(xc as usize));
                [r, 0.0, 0.0, 1.0]
            }
        }
    }
}

#[inline] fn u8_to_unorm(b: u8) -> f32 { b as f32 / 255.0 }

#[inline]
fn apply_wrap(c: i32, n: i32, mode: WrapMode) -> i32 {
    match mode {
        WrapMode::ClampToEdge => c.clamp(0, n - 1),
        WrapMode::Repeat => {
            // Rust's `%` follows the dividend's sign; we
            // want Euclidean mod so negatives wrap forward.
            ((c % n) + n) % n
        }
        WrapMode::Mirror => {
            // Triangle wave with period 2n.
            let period = 2 * n;
            let m = ((c % period) + period) % period;
            if m < n { m } else { period - 1 - m }
        }
    }
}

#[inline]
fn format_from_u32(v: u32) -> TexFormat {
    match v {
        0 => TexFormat::Rgba8Unorm,
        1 => TexFormat::Bgra8Unorm,
        2 => TexFormat::R8Unorm,
        // Defensive — a malformed descriptor falls back
        // to a recognisable garbage value rather than UB.
        _ => TexFormat::Rgba8Unorm,
    }
}

#[inline]
fn filter_from_u32(v: u32) -> FilterMode {
    if v == 1 { FilterMode::Linear } else { FilterMode::Nearest }
}

#[inline]
fn wrap_from_u32(v: u32) -> WrapMode {
    match v {
        1 => WrapMode::Repeat,
        2 => WrapMode::Mirror,
        _ => WrapMode::ClampToEdge,
    }
}

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 checkerboard, RGBA8 unorm: red, green / blue, white.
    fn checker() -> (Vec<u8>, TexDesc) {
        // Row-major, 4 bytes per pixel, stride = 2*4 = 8.
        let pixels: Vec<u8> = vec![
            255,   0,   0, 255,   // (0,0) red
              0, 255,   0, 255,   // (1,0) green
              0,   0, 255, 255,   // (0,1) blue
            255, 255, 255, 255,   // (1,1) white
        ];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: TexFormat::Rgba8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        (pixels, desc)
    }

    #[test]
    fn fetch_corners() {
        let (_pixels, desc) = checker();
        // 0,0 → red
        let p = fetch_texel_impl(&desc, 0, 0);
        assert!((p[0] - 1.0).abs() < 1e-6 && p[1] == 0.0 && p[2] == 0.0);
        // 1,1 → white
        let p = fetch_texel_impl(&desc, 1, 1);
        for k in 0..4 { assert!((p[k] - 1.0).abs() < 1e-6); }
    }

    #[test]
    fn nearest_sample_at_centre_is_exact_texel() {
        let (_pixels, desc) = checker();
        let samp = SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        // Centre of texel (0,0) sits at u=0.25, v=0.25
        // (texel size 0.5 in normalised coords on a 2x2).
        let p = sample_2d_impl(&desc, &samp, 0.25, 0.25);
        assert!((p[0] - 1.0).abs() < 1e-6); // red
        // Centre of (1,1) at u=0.75, v=0.75 → white.
        let p = sample_2d_impl(&desc, &samp, 0.75, 0.75);
        for k in 0..4 { assert!((p[k] - 1.0).abs() < 1e-6); }
    }

    #[test]
    fn bilinear_at_geometric_centre_averages_four() {
        let (_pixels, desc) = checker();
        let samp = SamplerDesc {
            mag_filter: FilterMode::Linear as u32,
            min_filter: FilterMode::Linear as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        // u=v=0.5 lands exactly at the four-texel meeting
        // point: x = 0.5*2 - 0.5 = 0.5 → fx=0.5, x0=0,x1=1.
        // Same for y. Output should be the equal-weight
        // average of (red, green, blue, white).
        let p = sample_2d_impl(&desc, &samp, 0.5, 0.5);
        // avg R = (1+0+0+1)/4 = 0.5; G = (0+1+0+1)/4 = 0.5;
        // B = (0+0+1+1)/4 = 0.5; A = 1.
        for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
            "lane {k}: got {}", p[k]); }
        assert!((p[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bgra_decodes_swapped() {
        // Same pixels as `checker` but the descriptor says
        // BGRA — fetch should swap R/B.
        let pixels: Vec<u8> = vec![
            255,   0,   0, 255,   // (0,0): BGRA → R from byte2=0, B from byte0=255 → blue
              0, 255,   0, 255,
              0,   0, 255, 255,   // (0,1): BGRA → R=255, B=0 → red
            255, 255, 255, 255,
        ];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: TexFormat::Bgra8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        let p0 = fetch_texel_impl(&desc, 0, 0);
        assert!(p0[0] == 0.0 && p0[2] == 1.0, "BGRA swap (0,0): {p0:?}");
        let p2 = fetch_texel_impl(&desc, 0, 1);
        assert!((p2[0] - 1.0).abs() < 1e-6 && p2[2] == 0.0,
                "BGRA swap (0,1): {p2:?}");
    }

    #[test]
    fn wrap_repeat() {
        // c=-1 with n=4, repeat → 3.
        assert_eq!(apply_wrap(-1, 4, WrapMode::Repeat), 3);
        // c=5 with n=4, repeat → 1.
        assert_eq!(apply_wrap(5, 4, WrapMode::Repeat), 1);
    }

    #[test]
    fn wrap_clamp() {
        assert_eq!(apply_wrap(-3, 4, WrapMode::ClampToEdge), 0);
        assert_eq!(apply_wrap(99, 4, WrapMode::ClampToEdge), 3);
    }

    #[test]
    fn wrap_mirror() {
        // n=4: indices flow 0,1,2,3,3,2,1,0,0,1,2,3,...
        // for c = 0..8 (one full mirror period).
        let n = 4;
        let expected = [0, 1, 2, 3, 3, 2, 1, 0];
        for (c, &e) in expected.iter().enumerate() {
            assert_eq!(apply_wrap(c as i32, n, WrapMode::Mirror), e,
                       "c={c}");
        }
        // And negative side: c=-1 → 0, c=-4 → 3, c=-5 → 3.
        assert_eq!(apply_wrap(-1, n, WrapMode::Mirror), 0);
        assert_eq!(apply_wrap(-4, n, WrapMode::Mirror), 3);
    }

    #[test]
    fn sample_cube_picks_right_face() {
        // 6 distinct 1x1 RGBA8 faces packed contiguously
        // (slice_bytes = 4 each):
        //   face 0 (+X) = red,   face 1 (-X) = green
        //   face 2 (+Y) = blue,  face 3 (-Y) = yellow
        //   face 4 (+Z) = cyan,  face 5 (-Z) = magenta
        let face_pixels: Vec<u8> = vec![
            255,   0,   0, 255,   // +X red
              0, 255,   0, 255,   // -X green
              0,   0, 255, 255,   // +Y blue
            255, 255,   0, 255,   // -Y yellow
              0, 255, 255, 255,   // +Z cyan
            255,   0, 255, 255,   // -Z magenta
        ];
        let tex = TexDesc {
            data: face_pixels.as_ptr(),
            width: 1, height: 1, stride_bytes: 4,
            format: TexFormat::Rgba8Unorm as u32,
            mip_count: 0, mip_descs: std::ptr::null(),
            depth: 6, slice_bytes: 4,
        };
        let samp = SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        // Direction vectors pointing along each principal axis.
        let cases: &[([f32; 3], [f32; 4])] = &[
            ([ 1.0,  0.0,  0.0], [1.0, 0.0, 0.0, 1.0]), // +X -> red
            ([-1.0,  0.0,  0.0], [0.0, 1.0, 0.0, 1.0]), // -X -> green
            ([ 0.0,  1.0,  0.0], [0.0, 0.0, 1.0, 1.0]), // +Y -> blue
            ([ 0.0, -1.0,  0.0], [1.0, 1.0, 0.0, 1.0]), // -Y -> yellow
            ([ 0.0,  0.0,  1.0], [0.0, 1.0, 1.0, 1.0]), // +Z -> cyan
            ([ 0.0,  0.0, -1.0], [1.0, 0.0, 1.0, 1.0]), // -Z -> magenta
        ];
        for &(dir, want) in cases {
            let mut got = [0.0f32; 4];
            unsafe {
                atrium_tex_sample_cube(
                    &tex as *const _, &samp as *const _,
                    dir[0], dir[1], dir[2], got.as_mut_ptr());
            }
            assert_eq!(got, want,
                "cube dir {dir:?}: got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn sample_2d_lod_indirects_through_mip_descs() {
        // Two distinct 1×1 RGBA8 mip levels: mip 0 = red,
        // mip 1 = blue.  Build a base TexDesc with mip_count
        // = 2 + mip_descs pointing at the per-mip array, then
        // call atrium_tex_sample_2d_lod at LOD=0 and LOD=1
        // and assert the right mip is selected.
        let mip0_pixels: Vec<u8> = vec![255, 0, 0, 255];
        let mip1_pixels: Vec<u8> = vec![0, 0, 255, 255];
        let mip_array: Vec<TexDesc> = vec![
            TexDesc {
                data: mip0_pixels.as_ptr(),
                width: 1, height: 1, stride_bytes: 4,
                format: TexFormat::Rgba8Unorm as u32,
                depth: 1, slice_bytes: 0,
                mip_count: 0, mip_descs: std::ptr::null(),
            },
            TexDesc {
                data: mip1_pixels.as_ptr(),
                width: 1, height: 1, stride_bytes: 4,
                format: TexFormat::Rgba8Unorm as u32,
                depth: 1, slice_bytes: 0,
                mip_count: 0, mip_descs: std::ptr::null(),
            },
        ];
        let base = TexDesc {
            data: mip0_pixels.as_ptr(),
            width: 1, height: 1, stride_bytes: 4,
            format: TexFormat::Rgba8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 2,
            mip_descs: mip_array.as_ptr(),
        };
        let samp = SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        let mut out_mip0 = [0.0f32; 4];
        let mut out_mip1 = [0.0f32; 4];
        unsafe {
            atrium_tex_sample_2d_lod(
                &base as *const _, &samp as *const _,
                0.5, 0.5, 0.0, out_mip0.as_mut_ptr());
            atrium_tex_sample_2d_lod(
                &base as *const _, &samp as *const _,
                0.5, 0.5, 1.0, out_mip1.as_mut_ptr());
        }
        assert_eq!(out_mip0, [1.0, 0.0, 0.0, 1.0], "LOD 0 -> red");
        assert_eq!(out_mip1, [0.0, 0.0, 1.0, 1.0], "LOD 1 -> blue");
        // LOD out of range falls back to base.
        let mut out_oob = [0.0f32; 4];
        unsafe {
            atrium_tex_sample_2d_lod(
                &base as *const _, &samp as *const _,
                0.5, 0.5, 5.0, out_oob.as_mut_ptr());
        }
        assert_eq!(out_oob, [1.0, 0.0, 0.0, 1.0],
            "out-of-range LOD falls back to base (red)");
    }

    #[test]
    fn descriptor_table_layout_round_trips() {
        // Two slots: binding 0 → (tex_a, samp_a), binding 1
        // → (tex_b, samp_b). After writing, decode each
        // 8-byte field as a u64 and verify it matches the
        // original pointer's `as usize` value.
        let (pixels, mut tex_a) = checker();
        // Suppress unused warnings — we hand `pixels` alive
        // through the borrow on tex_a.data.
        let _ = &pixels;
        let mut tex_b = tex_a;
        tex_b.width = 99; // make the descriptors distinguishable
        let samp_a = SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        };
        let mut samp_b = samp_a;
        samp_b.mag_filter = FilterMode::Linear as u32;

        // Mutate-then-take-pointer so the addresses are
        // stable for the rest of the test.
        let tex_a_ptr  = &tex_a  as *const TexDesc;
        let tex_b_ptr  = &tex_b  as *const TexDesc;
        let samp_a_ptr = &samp_a as *const SamplerDesc;
        let samp_b_ptr = &samp_b as *const SamplerDesc;
        // `tex_a` / `samp_*` aren't actually mutated past
        // this point — silence the `mut` lint.
        let _ = &mut tex_a;

        let mut buf = descriptor_table_buffer(2);
        assert_eq!(buf.len(), UNIFORMS_DESC_BASE + 2 * DESC_SLOT_BYTES);
        unsafe {
            write_helper_pointers(&mut buf,
                atrium_tex_sample_2d, atrium_tex_fetch_2d,
                atrium_tex_sample_2d_lod,
                atrium_tex_sample_2d_array,
                atrium_tex_sample_cube);
            write_descriptor_slot(&mut buf, 0, tex_a_ptr, samp_a_ptr);
            write_descriptor_slot(&mut buf, 1, tex_b_ptr, samp_b_ptr);
        }

        let read_u64 = |off: usize| u64::from_le_bytes(
            buf[off..off + 8].try_into().unwrap());
        // Helper header.
        assert_eq!(read_u64(0), atrium_tex_sample_2d as usize as u64);
        assert_eq!(read_u64(8), atrium_tex_fetch_2d  as usize as u64);
        // Descriptor slot 0 (binding 0).
        assert_eq!(read_u64(UNIFORMS_DESC_BASE),     tex_a_ptr  as usize as u64);
        assert_eq!(read_u64(UNIFORMS_DESC_BASE + 8), samp_a_ptr as usize as u64);
        // Descriptor slot 1 (binding 1).
        assert_eq!(read_u64(UNIFORMS_DESC_BASE + 16),
            tex_b_ptr  as usize as u64);
        assert_eq!(read_u64(UNIFORMS_DESC_BASE + 24),
            samp_b_ptr as usize as u64);
    }

    #[test]
    fn r8_replicates_to_red_alpha_one() {
        let pixels: Vec<u8> = vec![128, 200, 50, 255];
        let desc = TexDesc {
            data: pixels.as_ptr(),
            width: 4, height: 1, stride_bytes: 4,
            format: TexFormat::R8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        let p = fetch_texel_impl(&desc, 1, 0);
        assert!((p[0] - 200.0 / 255.0).abs() < 1e-6);
        assert!(p[1] == 0.0 && p[2] == 0.0);
        assert!((p[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn storage_image_rgba8_write_then_read_roundtrips() {
        // 2×2 Rgba8Unorm image.
        let mut data = vec![0u8; 2 * 2 * 4];
        let img = ImageDesc {
            data: data.as_mut_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: StorageFormat::Rgba8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        // Write a distinct colour at (1,1).
        image_write_impl(&img, 1, 1, [1.0, 0.5, 0.25, 0.0]);
        let got = image_read_impl(&img, 1, 1);
        // u8 round-trip: 0.5 -> 128/255, 0.25 -> 64/255.
        assert!((got[0] - 1.0).abs() < 1e-6);
        assert!((got[1] - 128.0 / 255.0).abs() < 1e-6);
        assert!((got[2] -  64.0 / 255.0).abs() < 1e-6);
        assert!(got[3] == 0.0);
        // Untouched texel (0,0) stays zero.
        assert_eq!(image_read_impl(&img, 0, 0), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn storage_image_rgba32f_is_exact() {
        let mut data = vec![0u8; 1 * 1 * 16];
        let img = ImageDesc {
            data: data.as_mut_ptr(),
            width: 1, height: 1, stride_bytes: 16,
            format: StorageFormat::Rgba32Float as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        let v = [3.14159_f32, -2.71828, 1e9, -0.0];
        image_write_impl(&img, 0, 0, v);
        assert_eq!(image_read_impl(&img, 0, 0), v);
    }

    #[test]
    fn storage_image_r32f_keeps_red_only() {
        let mut data = vec![0u8; 4];
        let img = ImageDesc {
            data: data.as_mut_ptr(),
            width: 1, height: 1, stride_bytes: 4,
            format: StorageFormat::R32Float as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        image_write_impl(&img, 0, 0, [42.5, 99.0, 99.0, 99.0]);
        let got = image_read_impl(&img, 0, 0);
        assert_eq!(got[0], 42.5);
        assert_eq!(got[1], 0.0);
        assert_eq!(got[2], 0.0);
        assert_eq!(got[3], 1.0);
    }

    #[test]
    fn storage_image_read_clamps_out_of_range() {
        let mut data = vec![0u8; 2 * 2 * 4];
        let img = ImageDesc {
            data: data.as_mut_ptr(),
            width: 2, height: 2, stride_bytes: 8,
            format: StorageFormat::Rgba8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        image_write_impl(&img, 1, 1, [1.0, 1.0, 1.0, 1.0]);
        // (5, 5) clamps to (1, 1).
        assert_eq!(image_read_impl(&img, 5, 5), image_read_impl(&img, 1, 1));
    }

    #[test]
    fn storage_image_3d_write_then_read_roundtrips() {
        // 2x2x2 R32Float image: write a distinct value at
        // each of the 8 texels, then read back through the
        // 3D path and verify slice_bytes folding works.
        let (w, h, d) = (2u32, 2u32, 2u32);
        let mut data = vec![0u8; (w * h * d * 4) as usize];
        let img = ImageDesc {
            data: data.as_mut_ptr(),
            width: w, height: h, stride_bytes: w * 4,
            format: StorageFormat::R32Float as u32,
            depth: d, slice_bytes: w * h * 4,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        for z in 0..d {
            for y in 0..h {
                for x in 0..w {
                    let v = (z * w * h + y * w + x) as f32 + 0.5;
                    image_write_impl_3d(
                        &img, x, y, z, [v, 0.0, 0.0, 0.0]);
                }
            }
        }
        for z in 0..d {
            for y in 0..h {
                for x in 0..w {
                    let got = image_read_impl_3d(&img, x, y, z);
                    let want = (z * w * h + y * w + x) as f32 + 0.5;
                    assert_eq!(got[0], want,
                        "texel ({x},{y},{z})");
                }
            }
        }
    }

    #[test]
    fn image_table_builder_round_trips() {
        let mut img = ImageDesc {
            data: std::ptr::null_mut(),
            width: 8, height: 8, stride_bytes: 32,
            format: StorageFormat::Rgba8Unorm as u32,
            depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
        };
        let mut buf = image_table_buffer(2);
        assert_eq!(buf.len(), IMG_TABLE_DESC_BASE + 2 * IMG_DESC_SLOT_BYTES);
        unsafe {
            write_image_helper_pointers(
                &mut buf,
                atrium_img_read_2d, atrium_img_write_2d,
                atrium_img_read_3d, atrium_img_write_3d,
                atrium_img_read_2d_lod, atrium_img_write_2d_lod,
                atrium_img_read_3d_lod, atrium_img_write_3d_lod);
            write_image_descriptor_slot(&mut buf, 1, &mut img as *const _);
        }
        let read_u64 = |off: usize| -> u64 {
            u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
        };
        assert_eq!(read_u64( 0), atrium_img_read_2d  as usize as u64);
        assert_eq!(read_u64( 8), atrium_img_write_2d as usize as u64);
        assert_eq!(read_u64(16), atrium_img_read_3d  as usize as u64);
        assert_eq!(read_u64(24), atrium_img_write_3d as usize as u64);
        assert_eq!(read_u64(32), atrium_img_read_2d_lod  as usize as u64);
        assert_eq!(read_u64(40), atrium_img_write_2d_lod as usize as u64);
        assert_eq!(read_u64(48), atrium_img_read_3d_lod  as usize as u64);
        assert_eq!(read_u64(56), atrium_img_write_3d_lod as usize as u64);
        assert_eq!(read_u64(IMG_TABLE_DESC_BASE + IMG_DESC_SLOT_BYTES),
            &img as *const ImageDesc as usize as u64);
    }
}
