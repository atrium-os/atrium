//! Tier-2 shader registry.
//!
//! Bridges the wire protocol's per-session shader-upload
//! flow to atrium-spv-loader's AOT compile + dlopen
//! pipeline. When the eventual `Tier2Backend` lands
//! (Phase 2 v5d+), each session will own one of these
//! and call [`Tier2Registry::register`] inside its
//! `handle_shader_upload` path; the resulting
//! `Tier2ShaderId` becomes the daemon-side resource id
//! that pipeline + draw ops look up to find the actual
//! compiled `atrium_fs_main` / `atrium_vs_main` function
//! pointers.
//!
//! This commit lands the registry as a standalone
//! primitive that exercises the full atrium-spv pipeline
//! end-to-end inside the aqueduct-gpu-host crate
//! (frontend → backend → loader → dlopen → call). The
//! plumbing into `Session::handle_shader_upload` lands
//! in a follow-up step once the wire ops for "give me
//! a Tier-2-compiled shader" are finalised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atrium_spv_loader::{LoadError, LoaderConfig, LoadedShader, ShaderCache};

/// Daemon-side id for a registered Tier-2 shader.
///
/// Newtype around a `u64` so we never confuse it with
/// the wire-protocol's `ResourceId` (which is namespaced
/// to the ICD runtime). The registry's internal counter
/// is opaque to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tier2ShaderId(pub u64);

/// Registry of Tier-2-compiled shaders, keyed by content
/// hash. Internally wraps an atrium-spv-loader
/// [`ShaderCache`] (which does the SHA-256 keyed
/// .dylib/.so caching to disk).
pub struct Tier2Registry {
    cache: Arc<ShaderCache>,
    /// hash → assigned Tier2ShaderId. Lets repeat uploads
    /// of the same SPIR-V map to the same id without
    /// re-invoking the compiler.
    by_hash: Mutex<HashMap<String, Tier2ShaderId>>,
    /// Tier2ShaderId → loaded shader. Strong references
    /// keep the dlopened library alive for the registry's
    /// lifetime.
    by_id: Mutex<HashMap<Tier2ShaderId, Arc<LoadedShader>>>,
    /// Monotonic id allocator.
    next_id: Mutex<u64>,
}

impl Tier2Registry {
    /// Construct a registry backed by the given loader
    /// config. The config supplies the on-disk cache
    /// directory and the path to the `atrium-spv-compile`
    /// helper binary.
    pub fn new(config: LoaderConfig) -> Self {
        Self {
            cache: Arc::new(ShaderCache::new(config)),
            by_hash: Mutex::new(HashMap::new()),
            by_id: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Register a SPIR-V module. Compiles it via the
    /// underlying loader (cache miss → spawn the compiler
    /// subprocess), dlopens the result, and returns an
    /// opaque id usable for later lookups.
    ///
    /// Idempotent: re-registering the same bytes returns
    /// the same id without recompiling.
    pub fn register(&self, spirv: &[u8]) -> Result<Tier2ShaderId, LoadError> {
        let hash = ShaderCache::hash(spirv);
        if let Some(id) = self.by_hash.lock().unwrap().get(&hash).copied() {
            return Ok(id);
        }
        let loaded = self.cache.load_or_compile(spirv)?;
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = Tier2ShaderId(*n);
            *n += 1;
            id
        };
        self.by_hash.lock().unwrap().insert(hash, id);
        self.by_id.lock().unwrap().insert(id, loaded);
        Ok(id)
    }

    /// Look up a registered shader. `None` if the id has
    /// never been issued (or was forgotten).
    pub fn get(&self, id: Tier2ShaderId) -> Option<Arc<LoadedShader>> {
        self.by_id.lock().unwrap().get(&id).cloned()
    }

    /// Forget a registered shader. The dlopened library
    /// stays alive as long as any other Arc clones exist.
    /// Subsequent `register` of the same bytes will
    /// re-issue a fresh id.
    pub fn forget(&self, id: Tier2ShaderId) {
        if let Some(_loaded) = self.by_id.lock().unwrap().remove(&id) {
            // Drop the by_hash mapping too (linear search
            // because we don't index the other direction).
            let mut by_hash = self.by_hash.lock().unwrap();
            by_hash.retain(|_h, v| *v != id);
        }
    }

    /// Render a fragment shader into a flat RGBA8 image
    /// buffer.
    ///
    /// Walks every pixel of a `width × height` image,
    /// invoking `atrium_fs_main` once per pixel with the
    /// supplied push-constant + uniform buffers. The
    /// shader's output `vec4` is converted from float
    /// `[0, 1]` to `u8` (saturating cast) and written into
    /// `pixels` as RGBA bytes in row-major order.
    ///
    /// `pixels.len()` must equal `width * height * 4`.
    ///
    /// This is the simplest possible Tier-2 execution
    /// primitive: no geometry, no interpolated varyings,
    /// no depth. It corresponds exactly to drawing a full-
    /// screen quad with a constant-vertex fragment shader.
    /// Real submit_frame integration with rasterised
    /// geometry lands in a follow-up step.
    pub fn fill_image_fragment(
        &self,
        shader_id: Tier2ShaderId,
        push_constants: &[u8],
        uniforms: &[u8],
        width: u32,
        height: u32,
        pixels: &mut [u8],
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len, got: pixels.len(),
            });
        }
        let loaded = self.get(shader_id)
            .ok_or(Tier2ExecError::UnknownShader(shader_id))?;
        let fs_main = loaded.entry_points.fs_main
            .ok_or(Tier2ExecError::NotAFragmentShader)?;

        for y in 0..height {
            for x in 0..width {
                let mut out_color = [0.0f32; 4];
                let mut out_depth = 0.0f32;
                // SAFETY: fs_main is a dlopened C ABI
                // function; the ShaderRecord guarantees its
                // signature matches FsMain (checked at
                // dlopen time by atrium-spv-loader). Input
                // pointers are non-null only when their
                // buffers are non-empty.
                unsafe {
                    let pc_ptr = if push_constants.is_empty() {
                        std::ptr::null()
                    } else {
                        push_constants.as_ptr()
                    };
                    let uni_ptr = if uniforms.is_empty() {
                        std::ptr::null()
                    } else {
                        uniforms.as_ptr()
                    };
                    fs_main(
                        std::ptr::null(), uni_ptr, pc_ptr,
                        x as f32 + 0.5, y as f32 + 0.5, 0.0, 1.0,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                    );
                }
                let idx = ((y as usize) * (width as usize) + (x as usize)) * 4;
                pixels[idx    ] = f32_to_u8(out_color[0]);
                pixels[idx + 1] = f32_to_u8(out_color[1]);
                pixels[idx + 2] = f32_to_u8(out_color[2]);
                pixels[idx + 3] = f32_to_u8(out_color[3]);
            }
        }
        Ok(())
    }

    /// Rasterise a single triangle through a VS+FS pair into
    /// the supplied RGBA8 image buffer.
    ///
    /// Tier-2 rasterizer phases R.1 (geometry → pixels) and
    /// R.2 (perspective-correct varying interpolation).  Per
    /// `docs/spec/tier2-renderer.md` §8 and `RUNBOOK.md`
    /// "Next big arc — scoped: tier-2 rasterizer":
    ///
    /// * **Vertex shading** — call `atrium_vs_main` once per
    ///   vertex with that vertex's attribute buffer; collect
    ///   3 clip-space `gl_Position`s.
    /// * **Perspective divide** — NDC = (x/w, y/w, z/w);
    ///   cache `1/w` for perspective-correct interpolation.
    /// * **Viewport mapping** — Vulkan convention (y NOT
    ///   flipped); NDC → screen.
    /// * **Pineda edge functions + bbox**.  Pixel is inside
    ///   iff all three edge values have the same sign
    ///   (handles both windings — we don't bother with a
    ///   winding convention here).
    /// * **Perspective-correct varying interpolation** —
    ///   given the caller's `varyings_per_vertex` buffers
    ///   (an array of `varying_f32_count` `f32` lanes per
    ///   vertex), compute at each inside pixel:
    ///       v(P) = Σ_i b_i * (v_i / w_i)   /   Σ_i b_i * (1 / w_i)
    ///   where `b_i` are normalised barycentric coordinates
    ///   (`edge_i / total_edge_sum`).
    /// * **Fragment shading** — call `atrium_fs_main` with
    ///   the interpolated varying buffer + `gl_FragCoord =
    ///   (cx, cy, interp_z, 1/interp_inv_w)`.
    ///
    /// R.3+ adds depth test, clipping, blending.
    ///
    /// `varyings_per_vertex` is **caller-supplied**, not
    /// captured from the VS's `out_varyings` write.  That's
    /// a temporary scaffolding: the backends today route
    /// every Vertex Output `OpStore` to `out_position`
    /// (cranelift `src/lib.rs:1648`, bespoke `src/lib.rs:2412`)
    /// because the dispatch on `BuiltIn` vs `Location`
    /// decoration is queued as "vertex phase 4+" backend
    /// work.  Once that lands, a production path will
    /// capture each VS invocation's `out_varyings` buffer
    /// and drop the caller-supplied parameter; the
    /// interpolation math here is unchanged.
    pub fn fill_image_triangle(
        &self,
        vs_shader_id: Tier2ShaderId,
        fs_shader_id: Tier2ShaderId,
        draw: &DrawTriangle<'_>,
        width: u32,
        height: u32,
        pixels: &mut [u8],
        mut depth_buffer: Option<&mut [f32]>,
    ) -> Result<(), Tier2ExecError> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected_len {
            return Err(Tier2ExecError::BadPixelsLen {
                expected: expected_len,
                got: pixels.len(),
            });
        }
        // R.3 — depth buffer.  Caller-allocated parallel to
        // `pixels`; length is exactly `width * height` f32s.
        // None ⇒ no depth test or write (R.1 / R.2 behaviour).
        let pixel_count = (width as usize) * (height as usize);
        if let Some(db) = depth_buffer.as_deref() {
            if db.len() != pixel_count {
                return Err(Tier2ExecError::BadDepthBufferLen {
                    expected: pixel_count,
                    got: db.len(),
                });
            }
        }
        let varying_bytes = draw.varying_f32_count * 4;
        for i in 0..3 {
            if draw.varyings_per_vertex[i].len() != varying_bytes {
                return Err(Tier2ExecError::BadVaryingBufferLen {
                    vertex: i,
                    expected: varying_bytes,
                    got: draw.varyings_per_vertex[i].len(),
                });
            }
        }
        let vs_loaded = self.get(vs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(vs_shader_id))?;
        let vs_main = vs_loaded.entry_points.vs_main
            .ok_or(Tier2ExecError::NotAVertexShader)?;
        let fs_loaded = self.get(fs_shader_id)
            .ok_or(Tier2ExecError::UnknownShader(fs_shader_id))?;
        let fs_main = fs_loaded.entry_points.fs_main
            .ok_or(Tier2ExecError::NotAFragmentShader)?;

        let uni_ptr = if draw.uniforms.is_empty() {
            std::ptr::null()
        } else { draw.uniforms.as_ptr() };
        let pc_ptr = if draw.push_constants.is_empty() {
            std::ptr::null()
        } else { draw.push_constants.as_ptr() };

        // ── Step 1: vertex shading.  3 invocations.
        let mut clip_positions: [[f32; 4]; 3] = [[0.0; 4]; 3];
        let mut vary_scratch = [0u8; 256];
        let mut clip_dist = [0.0f32; 8];
        for i in 0..3 {
            let attr_ptr = if draw.vertex_attrs[i].is_empty() {
                std::ptr::null()
            } else { draw.vertex_attrs[i].as_ptr() };
            // SAFETY: vs_main is a dlopened C-ABI function;
            // the ShaderRecord guarantees its signature
            // matches VsMain (atrium-spv-loader checked it at
            // open time).  All pointers are valid for at least
            // the lifetime of this call.
            unsafe {
                vs_main(
                    attr_ptr,
                    std::ptr::null(),       // in_attr_strides — unused
                    uni_ptr,
                    pc_ptr,
                    i as u32, 0,
                    &mut clip_positions[i] as *mut [f32; 4],
                    vary_scratch.as_mut_ptr(),
                    clip_dist.as_mut_ptr(),
                );
            }
        }

        // ── Step 2: perspective divide + cache 1/w per vertex.
        // R.2's first deliverable.  After this, ndc[i] holds
        // (x/w, y/w, z/w) and inv_w[i] holds 1/w_i.
        //
        // Guard against w == 0 (degenerate / behind-camera);
        // any such vertex collapses the triangle, which the
        // edge-function inside test will catch as
        // zero-area, so we treat 1/0 as 0 and continue.
        let mut ndc: [[f32; 3]; 3] = [[0.0; 3]; 3];
        let mut inv_w: [f32; 3] = [0.0; 3];
        for i in 0..3 {
            let w = clip_positions[i][3];
            let iw = if w == 0.0 { 0.0 } else { 1.0 / w };
            inv_w[i] = iw;
            ndc[i][0] = clip_positions[i][0] * iw;
            ndc[i][1] = clip_positions[i][1] * iw;
            ndc[i][2] = clip_positions[i][2] * iw;
        }

        // ── Step 3: viewport mapping (NDC → screen).
        // Vulkan convention: y NOT flipped, so screen y=0
        // corresponds to NDC y=-1.
        let fw = width as f32;
        let fh = height as f32;
        let mut screen: [(f32, f32); 3] = [(0.0, 0.0); 3];
        for i in 0..3 {
            screen[i] = (
                (ndc[i][0] + 1.0) * 0.5 * fw,
                (ndc[i][1] + 1.0) * 0.5 * fh,
            );
        }

        // ── Step 4: pre-compute attr/w per vertex for every
        // varying lane.  Done once per triangle so the inner
        // pixel loop does only the barycentric weighting +
        // final divide by interpolated (1/w).
        let n = draw.varying_f32_count;
        let mut varying_over_w: [Vec<f32>; 3] = [
            vec![0.0f32; n],
            vec![0.0f32; n],
            vec![0.0f32; n],
        ];
        for i in 0..3 {
            for k in 0..n {
                let off = k * 4;
                let bytes = [
                    draw.varyings_per_vertex[i][off],
                    draw.varyings_per_vertex[i][off + 1],
                    draw.varyings_per_vertex[i][off + 2],
                    draw.varyings_per_vertex[i][off + 3],
                ];
                let f = f32::from_le_bytes(bytes);
                varying_over_w[i][k] = f * inv_w[i];
            }
        }

        // ── Step 5: screen-space bbox, clamped to viewport.
        let min_x = screen.iter().map(|p| p.0)
            .fold(f32::INFINITY, f32::min).max(0.0).floor() as i32;
        let max_x = screen.iter().map(|p| p.0)
            .fold(f32::NEG_INFINITY, f32::max).min(fw - 1.0).ceil() as i32;
        let min_y = screen.iter().map(|p| p.1)
            .fold(f32::INFINITY, f32::min).max(0.0).floor() as i32;
        let max_y = screen.iter().map(|p| p.1)
            .fold(f32::NEG_INFINITY, f32::max).min(fh - 1.0).ceil() as i32;
        if min_x > max_x || min_y > max_y {
            return Ok(());     // triangle fully off-screen
        }

        // ── Step 6: Pineda edge functions.  Pre-compute
        // total triangle area for barycentric normalisation;
        // the three edge values at any pixel sum to this
        // constant.
        let (a, b, c) = (screen[0], screen[1], screen[2]);
        let edge = |ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32| -> f32 {
            (bx - ax) * (py - ay) - (by - ay) * (px - ax)
        };
        // Total signed 2-area of triangle (A, B, C).  Zero
        // for degenerate triangles; bary normalisation skips
        // the divide when total is 0.
        let total_edge = edge(a.0, a.1, b.0, b.1, c.0, c.1);

        // ── Step 7: pixel loop.
        let mut interp_buf: Vec<u8> = vec![0u8; varying_bytes];
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let cx = px as f32 + 0.5;
                let cy = py as f32 + 0.5;
                let we0 = edge(b.0, b.1, c.0, c.1, cx, cy);
                let we1 = edge(c.0, c.1, a.0, a.1, cx, cy);
                let we2 = edge(a.0, a.1, b.0, b.1, cx, cy);
                let inside_pos = we0 >= 0.0 && we1 >= 0.0 && we2 >= 0.0;
                let inside_neg = we0 <= 0.0 && we1 <= 0.0 && we2 <= 0.0;
                if !(inside_pos || inside_neg) { continue; }

                // Normalised barycentrics.  Skip on degenerate
                // triangles; the edge-function inside test
                // above only fires for zero-area triangles
                // when a pixel centre happens to lie exactly
                // on the collinear edge, which is mostly
                // moot.
                if total_edge == 0.0 { continue; }
                let b0 = we0 / total_edge;
                let b1 = we1 / total_edge;
                let b2 = we2 / total_edge;

                // Perspective-correct interpolation.
                //   interp_inv_w = Σ b_i * (1/w_i)
                //   interp(attr) = Σ b_i * (attr_i/w_i)  /  interp_inv_w
                let interp_inv_w =
                    b0 * inv_w[0] + b1 * inv_w[1] + b2 * inv_w[2];
                // Divide once per pixel, not once per lane.
                let one_over_interp_inv_w = if interp_inv_w == 0.0 {
                    0.0
                } else {
                    1.0 / interp_inv_w
                };
                for k in 0..n {
                    let sow = b0 * varying_over_w[0][k]
                            + b1 * varying_over_w[1][k]
                            + b2 * varying_over_w[2][k];
                    let v = sow * one_over_interp_inv_w;
                    interp_buf[k*4 .. k*4 + 4]
                        .copy_from_slice(&v.to_le_bytes());
                }
                // Interpolated depth (NDC z).
                let interp_z =
                    b0 * ndc[0][2] + b1 * ndc[1][2] + b2 * ndc[2][2];

                // R.3 — depth test + write.  Default
                // comparison is LESS (incoming fragment passes
                // iff its z is strictly less than the stored
                // value, i.e. closer to the near plane in
                // Vulkan NDC where z ∈ [0, 1] with 0 = near).
                // On pass: write the new z and continue to
                // shading; on fail: skip the FS call entirely
                // (early-z elision -- the FS shouldn't run
                // for occluded pixels in R.3's simple opaque
                // pipeline).  R.5's blending arc revisits the
                // "shade-then-test" vs "test-then-shade" order
                // for shaders that write `discard` or
                // depth-replace.
                let pixel_lin = (py as usize) * (width as usize)
                    + (px as usize);
                if let Some(db) = depth_buffer.as_mut() {
                    if interp_z >= db[pixel_lin] { continue; }
                    db[pixel_lin] = interp_z;
                }

                let mut out_color = [0.0f32; 4];
                let mut out_depth = 0.0f32;
                let in_varyings_ptr = if n == 0 {
                    std::ptr::null()
                } else { interp_buf.as_ptr() };
                // SAFETY: same as VS above.
                unsafe {
                    fs_main(
                        in_varyings_ptr,
                        uni_ptr,
                        pc_ptr,
                        cx, cy, interp_z, one_over_interp_inv_w,
                        0,
                        out_color.as_mut_ptr(),
                        &mut out_depth,
                    );
                }
                let idx = pixel_lin * 4;
                pixels[idx]     = f32_to_u8(out_color[0]);
                pixels[idx + 1] = f32_to_u8(out_color[1]);
                pixels[idx + 2] = f32_to_u8(out_color[2]);
                pixels[idx + 3] = f32_to_u8(out_color[3]);
            }
        }
        Ok(())
    }
}

/// Draw-call parameters for [`Tier2Registry::fill_image_triangle`].
///
/// Bundling these into a struct so future rasterizer phases
/// (R.3 depth, R.4 clipping, R.5 blending, ...) can grow
/// fields without breaking every caller's argument order.
#[derive(Debug, Default, Clone, Copy)]
pub struct DrawTriangle<'a> {
    /// Per-vertex attribute buffers fed to the vertex shader.
    /// Layout is dictated by the VS's SPIR-V `Input`
    /// variables (typically a 12-byte `vec3` position at
    /// `Location=0`, contiguous f32 lanes for later
    /// locations).  Empty slices map to a null
    /// `in_attributes` parameter into `atrium_vs_main`.
    pub vertex_attrs: [&'a [u8]; 3],

    /// Per-vertex varying buffers consumed by the
    /// rasterizer's interpolator and fed to the FS as
    /// `in_varyings`.  See `fill_image_triangle`'s docstring
    /// for the temporary-scaffolding rationale (the
    /// backends' VS Output dispatch needs to learn the
    /// BuiltIn-vs-Location split before these can be
    /// captured from `out_varyings` directly).  Each slice
    /// must be exactly `varying_f32_count * 4` bytes long.
    pub varyings_per_vertex: [&'a [u8]; 3],

    /// Number of `f32` lanes in each per-vertex varying
    /// buffer.  The interpolator treats the buffer as that
    /// many contiguous little-endian `f32`s; the FS reads
    /// them via its `Input` `Location=N` decorations.
    pub varying_f32_count: usize,

    /// Uniform buffer shared across VS + FS invocations.
    /// Layout per the shaders' `Uniform`-storage
    /// declarations.  Empty → null.
    pub uniforms: &'a [u8],

    /// Push-constant buffer shared across VS + FS
    /// invocations.  Layout per the shaders' `PushConstant`
    /// declarations.  Empty → null.
    pub push_constants: &'a [u8],
}

/// Saturating float-to-u8 conversion matching the standard
/// sRGB framebuffer convention.
fn f32_to_u8(v: f32) -> u8 {
    if v.is_nan() { 0 }
    else if v <= 0.0 { 0 }
    else if v >= 1.0 { 255 }
    else { (v * 255.0 + 0.5) as u8 }
}

/// Errors from [`Tier2Registry::fill_image_fragment`].
#[derive(Debug, thiserror::Error)]
pub enum Tier2ExecError {
    /// `pixels.len()` didn't match `width * height * 4`.
    #[error("pixels buffer length {got} doesn't match expected {expected}")]
    BadPixelsLen {
        /// Required length.
        expected: usize,
        /// Caller-provided length.
        got: usize,
    },
    /// `shader_id` not in the registry (never registered or
    /// forgotten).
    #[error("Tier-2 shader id {0:?} not in registry")]
    UnknownShader(Tier2ShaderId),
    /// The shader doesn't export `atrium_fs_main` (e.g.
    /// it's a vertex or compute shader).
    #[error("shader has no atrium_fs_main entry point")]
    NotAFragmentShader,
    /// The shader doesn't export `atrium_vs_main` (e.g.
    /// it's a fragment or compute shader).
    #[error("shader has no atrium_vs_main entry point")]
    NotAVertexShader,
    /// A per-vertex varying buffer's byte length didn't
    /// match `varying_f32_count * 4`.
    #[error("varyings_per_vertex[{vertex}] length {got} != expected {expected}")]
    BadVaryingBufferLen {
        /// Which of the 3 vertex buffers was wrong.
        vertex: usize,
        /// Required length (`varying_f32_count * 4`).
        expected: usize,
        /// Caller-supplied length.
        got: usize,
    },
    /// The supplied depth buffer's length didn't match
    /// `width * height` (one `f32` per pixel).
    #[error("depth_buffer length {got} != expected {expected}")]
    BadDepthBufferLen {
        /// Required length (`width * height`).
        expected: usize,
        /// Caller-supplied length.
        got: usize,
    },
}
